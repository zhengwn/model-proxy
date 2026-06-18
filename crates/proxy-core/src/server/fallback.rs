//! Fallback logic: when the active provider fails with a configured status code,
//! try other providers in the registry until one succeeds or max_attempts is reached.

use axum::response::Response;
use serde_json::Value;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use super::provider_dispatch::{get_dispatch, PreparedRequest, ResponseContext};
use super::state::NON_STREAM_REQUEST_TIMEOUT_SECS;
use crate::config::{ModelRoute, ProviderConfig};
use crate::error::Result;
use crate::ProviderRegistry;

/// The format of the original client request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputFormat {
    /// Request came from `/v1/messages` (Anthropic Messages API format)
    Anthropic,
    /// Request came from `/v1/chat/completions` (OpenAI Chat Completions format)
    OpenAI,
}

/// Context needed to attempt a fallback request.
pub(crate) struct FallbackContext<'a> {
    pub(crate) client: &'a reqwest::Client,
    pub(crate) raw_body_bytes: &'a [u8],
    pub(crate) global_routes: &'a [ModelRoute],
    pub(crate) request_id: &'a str,
    pub(crate) request_start: Instant,
    pub(crate) input_format: InputFormat,
    pub(crate) req_log: &'a mut crate::server::request_log::RequestLog,
}

/// Attempt fallback to other providers in the registry.
///
/// Returns `Some(response)` if a fallback provider succeeded, or `None` if all failed.
pub(crate) async fn try_fallback(
    ctx: FallbackContext<'_>,
    registry: &ProviderRegistry,
    current_name: &str,
    max_attempts: usize,
    original_status: u16,
) -> Option<Result<Response>> {
    let mut attempts = 1; // Already tried the active provider

    for fallback_provider in registry.list() {
        if attempts >= max_attempts {
            break;
        }
        if fallback_provider.name == current_name {
            continue;
        }

        attempts += 1;
        info!(
            request_id = ctx.request_id,
            failed_provider = current_name,
            fallback_provider = fallback_provider.name.as_str(),
            original_status,
            attempt = attempts,
            "尝试 Fallback 到备用 Provider"
        );

        let response = try_single_provider(
            ctx.client,
            ctx.raw_body_bytes,
            fallback_provider,
            ctx.global_routes,
            ctx.request_id,
            ctx.request_start,
            ctx.input_format,
            ctx.req_log,
        )
        .await;

        match response {
            Some(resp) => return Some(resp),
            None => continue,
        }
    }

    None
}

/// Try a single fallback provider. Returns `Some(response)` on success, `None` on failure.
///
/// Reuses the same `ProviderDispatch` abstraction as the primary request path,
/// so request preparation and response conversion stay consistent between the
/// two. Kiro providers are skipped (they have a dedicated dispatch path and
/// require auth that fallback does not carry).
#[allow(clippy::too_many_arguments)]
async fn try_single_provider(
    client: &reqwest::Client,
    raw_body_bytes: &[u8],
    provider: &ProviderConfig,
    global_routes: &[ModelRoute],
    request_id: &str,
    request_start: Instant,
    input_format: InputFormat,
    req_log: &mut crate::server::request_log::RequestLog,
) -> Option<Result<Response>> {
    // Kiro is not dispatchable through the generic path.
    let Some(dispatch) = get_dispatch(&provider.format) else {
        warn!(
            request_id,
            fallback_provider = provider.name.as_str(),
            "跳过 Kiro provider 作为 fallback 目标（需专用 dispatch 路径）"
        );
        return None;
    };

    let fallback_body: Value = match serde_json::from_slice(raw_body_bytes) {
        Ok(v) => v,
        Err(_) => return None,
    };

    // Prepare the request body via the same dispatch used on the primary path.
    let prepared: PreparedRequest = match input_format {
        InputFormat::Anthropic => {
            match dispatch.prepare_messages_body(fallback_body, provider, global_routes) {
                Ok(p) => p,
                Err(_) => return None,
            }
        }
        InputFormat::OpenAI => {
            match dispatch.prepare_completions_body(fallback_body, provider, global_routes) {
                Ok(p) => p,
                Err(_) => return None,
            }
        }
    };

    let is_stream = prepared.is_stream;

    let mut fallback_req = dispatch.build_request(client, provider, prepared.body.clone());
    if !is_stream {
        fallback_req = fallback_req.timeout(Duration::from_secs(NON_STREAM_REQUEST_TIMEOUT_SECS));
    }

    let fallback_upstream_start = Instant::now();

    let fallback_resp = match fallback_req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(
                request_id,
                fallback_provider = provider.name.as_str(),
                error = %e,
                "Fallback Provider 请求失败，继续尝试下一个"
            );
            return None;
        }
    };

    let fallback_headers_ms = fallback_upstream_start.elapsed().as_millis();

    if !fallback_resp.status().is_success() {
        warn!(
            request_id,
            fallback_provider = provider.name.as_str(),
            status = %fallback_resp.status(),
            "Fallback Provider 返回错误，继续尝试下一个"
        );
        return None;
    }

    info!(
        request_id,
        fallback_provider = provider.name.as_str(),
        "Fallback 成功"
    );

    req_log.provider = provider.name.clone();
    req_log.model = prepared.provider_model.clone();
    req_log.upstream_start = fallback_upstream_start;

    let stream_log_ctx = if is_stream {
        Some(req_log.to_stream_log_ctx())
    } else {
        None
    };

    let ctx = ResponseContext {
        request_id: request_id.to_string(),
        request_start,
        upstream_start: fallback_upstream_start,
        upstream_headers_ms: fallback_headers_ms,
        log_ctx: stream_log_ctx,
    };

    let response = match input_format {
        InputFormat::Anthropic => {
            dispatch
                .handle_messages_response(fallback_resp, prepared, ctx)
                .await
        }
        InputFormat::OpenAI => {
            dispatch
                .handle_completions_response(fallback_resp, prepared, ctx)
                .await
        }
    };

    if !is_stream && response.is_ok() {
        req_log.emit_success(fallback_headers_ms);
    }

    Some(response)
}
