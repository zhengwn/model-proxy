//! Fallback logic: when the active provider fails with a configured status code,
//! try other providers in the registry until one succeeds or max_attempts is reached.

use axum::response::Response;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::convert::anthropic_openai::request::{build_provider_request, prepare_body, prepare_chat_completions_body};
use crate::convert::anthropic_openai::response::{handle_non_stream, handle_non_stream_openai_output};
use crate::convert::anthropic_openai::stream::{handle_stream, handle_stream_openai_output};
use crate::convert::passthrough::{handle_non_stream_passthrough, handle_stream_passthrough};
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
    pub(crate) upstream_start: Instant,
    pub(crate) upstream_headers_ms: u128,
    pub(crate) input_format: InputFormat,
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
            ctx.upstream_start,
            ctx.upstream_headers_ms,
            ctx.input_format,
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
async fn try_single_provider(
    client: &reqwest::Client,
    raw_body_bytes: &[u8],
    provider: &ProviderConfig,
    global_routes: &[ModelRoute],
    request_id: &str,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
    input_format: InputFormat,
) -> Option<Result<Response>> {
    let fallback_body: Value = match serde_json::from_slice(raw_body_bytes) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let (fallback_body_json, fallback_is_stream, fallback_tool_name_map) = match input_format {
        InputFormat::Anthropic => match prepare_body(fallback_body, provider, global_routes) {
            Ok(v) => v,
            Err(_) => return None,
        },
        InputFormat::OpenAI => {
            match prepare_chat_completions_body(fallback_body, provider, global_routes) {
                Ok(v) => v,
                Err(_) => return None,
            }
        }
    };

    let fallback_model = fallback_body_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(provider.model.as_str())
        .to_string();

    let mut fallback_req = build_provider_request(client, provider, fallback_body_json);
    if !fallback_is_stream {
        fallback_req = fallback_req.timeout(Duration::from_secs(NON_STREAM_REQUEST_TIMEOUT_SECS));
    }

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

    let response = match (input_format, &provider.format) {
        // Anthropic input → OpenAI provider: convert response back to Anthropic
        (InputFormat::Anthropic, crate::config::ProviderFormat::Openai) => {
            if fallback_is_stream {
                handle_stream(
                    fallback_resp,
                    &fallback_model,
                    Arc::new(fallback_tool_name_map),
                    request_id.to_string(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    None,
                )
                .await
            } else {
                handle_non_stream(
                    fallback_resp,
                    &fallback_model,
                    &fallback_tool_name_map,
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
        // Anthropic input → Anthropic provider: passthrough
        (InputFormat::Anthropic, crate::config::ProviderFormat::Anthropic) => {
            if fallback_is_stream {
                handle_stream_passthrough(
                    fallback_resp,
                    request_id.to_string(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    None,
                )
                .await
            } else {
                handle_non_stream_passthrough(
                    fallback_resp,
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
        // OpenAI input → OpenAI provider: passthrough
        (InputFormat::OpenAI, crate::config::ProviderFormat::Openai) => {
            if fallback_is_stream {
                handle_stream_passthrough(
                    fallback_resp,
                    request_id.to_string(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    None,
                )
                .await
            } else {
                handle_non_stream_passthrough(
                    fallback_resp,
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
        // OpenAI input → Anthropic provider: convert response to OpenAI
        (InputFormat::OpenAI, crate::config::ProviderFormat::Anthropic) => {
            if fallback_is_stream {
                handle_stream_openai_output(
                    fallback_resp,
                    &fallback_model,
                    request_id.to_string(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    None,
                )
                .await
            } else {
                handle_non_stream_openai_output(
                    fallback_resp,
                    &fallback_model,
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
        // Kiro provider: passthrough (response is already in the correct format
        // since prepare_body/prepare_chat_completions_body handle the conversion)
        (_, crate::config::ProviderFormat::Kiro) => {
            // For Kiro fallback, the response is EventStream format
            // Since Kiro requires auth tokens that the fallback may not have,
            // this path primarily handles the case where the Kiro provider
            // was the original target and we're retrying after a transient error.
            // The response format depends on the input format.
            if fallback_is_stream {
                handle_stream_passthrough(
                    fallback_resp,
                    request_id.to_string(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    None,
                )
                .await
            } else {
                handle_non_stream_passthrough(
                    fallback_resp,
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
    };

    Some(response)
}
