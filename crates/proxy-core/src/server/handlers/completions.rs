//! `POST /v1/chat/completions` (OpenAI Chat Completions) proxy handler.

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::HeaderMap,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tracing::info;

use super::sanitize_upstream_error;
use crate::server::auth::{check_auth, check_auth_tenant, AuthResult};
use crate::server::fallback::{self, try_fallback, InputFormat};
use crate::server::kiro_handlers::handle_kiro_chat_completions;
use crate::server::provider_dispatch::{get_dispatch, ResponseContext};
use crate::server::request_log::RequestLog;
use crate::server::state::{
    elapsed_ms, next_request_id, RequestCompletionGuard, NON_STREAM_REQUEST_TIMEOUT_SECS,
};
use crate::error::{AppError, Result};

/// OpenAI-compatible endpoint: accepts OpenAI Chat Completions format requests.
/// Forwards directly to OpenAI-format providers, or returns an error for Anthropic-format providers.
pub async fn proxy_chat_completions(
    State(state): State<crate::server::state::AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let request_id = next_request_id();
    let request_start = Instant::now();
    let mut request_guard = RequestCompletionGuard::new(request_id.clone(), request_start);

    state.inc_total_requests();
    if let Some(ref metrics) = state.metrics {
        metrics.connection_start();
        request_guard.set_metrics(metrics.clone());
    }

    check_auth(&headers, &state.config)?;

    request_guard.set_phase("auth");

    request_guard.set_phase("read_body");
    let bytes = to_bytes(body, state.config.server.max_body_bytes)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("length limit") || msg.contains("LengthLimit") {
                AppError::PayloadTooLarge
            } else {
                AppError::Request(format!("Failed to read request body: {}", msg))
            }
        })?;

    let body_json: Value = serde_json::from_slice(&bytes)?;
    request_guard.set_phase("received_body");
    let is_stream = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let provider = state.current_provider();
    let model_routes = state.current_model_routes();

    // Apply model routing
    let requested_model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Kiro provider: convert OpenAI → Kiro → Anthropic → OpenAI
    if crate::server::provider_dispatch::is_self_managed_format(&provider.format) {
        let tenant_token = match check_auth_tenant(&headers, &state.config) {
            AuthResult::TenantOk { refresh_token } => Some(refresh_token),
            _ => None,
        };
        let metrics = state.metrics.clone();
        return match handle_kiro_chat_completions(
            state,
            body_json,
            requested_model,
            request_id.clone(),
            request_start,
            tenant_token,
        )
        .await
        {
            Ok(resp) => {
                if let Some(ref m) = metrics {
                    m.record_request(request_start.elapsed().as_millis() as u64);
                }
                Ok(resp)
            }
            Err(e) => {
                if let Some(ref m) = metrics {
                    m.record_error(request_start.elapsed().as_millis() as u64);
                }
                Err(e)
            }
        };
    }

    let global_routes = if state.is_model_routes_enabled() { model_routes.as_slice() } else { &[] };

    // Capture for logging
    let raw_request_body = String::from_utf8_lossy(&bytes).into_owned();
    let log_provider_name = provider.name.clone();

    info!(
        request_id = request_id.as_str(),
        requested_model = requested_model.as_str(),
        provider_format = ?provider.format,
        stream = is_stream,
        "收到 OpenAI 格式请求 (/v1/chat/completions)"
    );

    let upstream_start = Instant::now();

    // Use provider dispatch for format-specific handling
    let dispatch = get_dispatch(&provider.format)
        .expect("Kiro requests are handled before this point");

    // Prepare the body using the dispatch (handles OpenAI->Anthropic conversion if needed)
    request_guard.set_phase("prepare_body");
    let prepared = dispatch.prepare_completions_body(body_json, &provider, global_routes)?;
    let is_stream = prepared.is_stream;
    let log_model = prepared.provider_model.clone();

    // Construct RequestLog for this handler
    let mut req_log = RequestLog {
        collector: state.log_collector.clone(),
        request_id: request_id.clone(),
        method: "POST",
        path: "/v1/chat/completions",
        provider: log_provider_name.clone(),
        model: log_model.clone(),
        requested_model: requested_model.clone(),
        request_start,
        upstream_start,
        is_stream,
        raw_request_body: raw_request_body.clone(),
    };

    // Build and send the upstream request
    let mut req = dispatch.build_request(&state.client, &provider, prepared.body.clone());
    if !is_stream {
        req = req.timeout(Duration::from_secs(NON_STREAM_REQUEST_TIMEOUT_SECS));
    }

    // Acquire concurrency permit if configured
    request_guard.set_phase("send_upstream");
    let _permit = if let Some(ref sem) = state.concurrency_semaphore {
        match sem.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => {
                state.inc_failed_requests();
                return Err(AppError::TooManyRequests);
            }
        }
    } else {
        None
    };

    let upstream_resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            state.inc_failed_requests();
            req_log.emit_send_error(&e.to_string());
            return Err(AppError::Http(e));
        }
    };

    let status = upstream_resp.status();
    let upstream_headers_ms = elapsed_ms(upstream_start);
    request_guard.set_phase("received_upstream_headers");

    if !status.is_success() {
        let text = upstream_resp.text().await.unwrap_or_default();
        let status_code = status.as_u16();

        // Try fallback
        if state.config.fallback.enabled
            && state.config.fallback.on_status_codes.contains(&status_code)
        {
            let registry = state.registry.load();
            let current_name = provider.name.clone();
            let ctx = fallback::FallbackContext {
                client: &state.client,
                raw_body_bytes: raw_request_body.as_bytes(),
                global_routes,
                request_id: &request_id,
                request_start,
                input_format: InputFormat::OpenAI,
                req_log: &mut req_log,
            };
            if let Some(response) = try_fallback(
                ctx,
                &registry,
                &current_name,
                state.config.fallback.max_attempts,
                status_code,
            )
            .await
            {
                super::record_result_metrics(state.metrics.as_ref(), &response, request_start);
                request_guard.complete();
                return response;
            }
        }

        state.inc_failed_requests();
        req_log.emit_upstream_error(status_code, upstream_headers_ms, &text);

        return Ok((
            StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(json!({
                "error": {
                    "message": sanitize_upstream_error(status_code, &text),
                    "type": "upstream_error"
                }
            })),
        )
            .into_response());
    }

    // Handle successful response via dispatch
    request_guard.set_phase(if is_stream {
        "handle_stream_response"
    } else {
        "handle_non_stream_response"
    });

    let stream_log_ctx = if is_stream {
        Some(req_log.to_stream_log_ctx())
    } else {
        None
    };

    let response = dispatch
        .handle_completions_response(
            upstream_resp,
            prepared,
            ResponseContext {
                request_id: request_id.clone(),
                request_start,
                upstream_start,
                upstream_headers_ms,
                log_ctx: stream_log_ctx,
            },
        )
        .await;

    if !is_stream && response.is_ok() {
        req_log.emit_success(upstream_headers_ms);
    }

    // Record metrics at handler exit
    super::record_result_metrics(state.metrics.as_ref(), &response, request_start);
    request_guard.complete();
    response
}
