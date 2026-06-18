//! `POST /v1/messages` (Anthropic Messages API) proxy handler.

use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, State},
    http::HeaderMap,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use super::sanitize_upstream_error;
use crate::convert::anthropic_openai::request::{build_provider_request, prepare_body};
use crate::convert::utils::{message_count, tool_count, truncate_for_log};
use crate::server::auth::{check_auth, check_auth_tenant, AuthResult};
use crate::server::fallback::{self, try_fallback, InputFormat};
use crate::server::kiro_handlers::handle_kiro_messages;
use crate::server::provider_dispatch::{get_dispatch, ResponseContext};
use crate::server::request_log::RequestLog;
use crate::server::state::{
    elapsed_ms, next_request_id, RequestCompletionGuard, MAX_LOG_BODY_BYTES,
    NON_STREAM_REQUEST_TIMEOUT_SECS,
};
use crate::error::{AppError, Result};

pub async fn proxy_messages(
    State(state): State<crate::server::state::AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    body: Body,
) -> Result<Response> {
    let request_id = next_request_id();
    let request_start = Instant::now();
    let mut request_guard = RequestCompletionGuard::new(request_id.clone(), request_start);

    // Track total requests
    state.inc_total_requests();
    if let Some(ref metrics) = state.metrics {
        metrics.connection_start();
        request_guard.set_metrics(metrics.clone());
    }

    // Detect /cc/v1/messages path for buffered streaming mode
    let is_buffered_path = uri.path().starts_with("/cc/");

    request_guard.set_phase("auth");
    check_auth(&headers, &state.config)?;

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

    let body_json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            if let Some(ref metrics) = state.metrics {
                metrics.record_error(request_start.elapsed().as_millis() as u64);
            }
            return Err(AppError::from(e));
        }
    };
    let requested_model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    request_guard.set_phase("received_body");
    info!(
        request_id = request_id.as_str(),
        body_bytes = bytes.len(),
        body_limit_bytes = state.config.server.max_body_bytes,
        model = requested_model.as_str(),
        stream = body_json
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        messages = message_count(&body_json),
        tools = tool_count(&body_json),
        "收到客户端请求"
    );

    // Capture the raw request body string for potential logging
    let raw_request_body = String::from_utf8_lossy(&bytes).into_owned();

    // Load the current active provider (lock-free via ArcSwap)
    let provider = state.current_provider();
    let model_routes = state.current_model_routes();

    // Kiro provider uses a completely different request/response flow
    if crate::server::provider_dispatch::is_self_managed_format(&provider.format) {
        // Extract tenant refresh token if multi-tenant auth
        let tenant_token = match check_auth_tenant(&headers, &state.config) {
            AuthResult::TenantOk { refresh_token } => Some(refresh_token),
            _ => None,
        };
        let metrics = state.metrics.clone();
        return match handle_kiro_messages(
            state,
            body_json,
            bytes,
            requested_model,
            raw_request_body,
            request_id,
            request_guard,
            request_start,
            tenant_token,
            is_buffered_path,
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

    request_guard.set_phase("prepare_body");
    let global_routes = if state.is_model_routes_enabled() { model_routes.as_slice() } else { &[] };
    let (body_json, is_stream, tool_name_map) = prepare_body(body_json, &provider, global_routes)?;
    let provider_model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(provider.model.as_str());
    let reasoning_effort = body_json
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let route_reasoning_effort = provider
        .resolve_route_reasoning_effort_with_routes(Some(requested_model.as_str()), global_routes)
        .unwrap_or("");

    info!(
        request_id = request_id.as_str(),
        requested_model = requested_model.as_str(),
        provider_model,
        route_reasoning_effort,
        reasoning_effort,
        provider_format = ?provider.format,
        stream = is_stream,
        messages = message_count(&body_json),
        tools = tool_count(&body_json),
        "准备转发上游请求"
    );

    // Capture provider name and model for logging
    let log_provider_name = provider.name.clone();
    let log_model = provider_model.to_string();

    // Construct RequestLog once for this handler invocation
    let mut req_log = RequestLog {
        collector: state.log_collector.clone(),
        request_id: request_id.clone(),
        method: "POST",
        path: "/v1/messages",
        provider: log_provider_name.clone(),
        model: log_model.clone(),
        requested_model: requested_model.clone(),
        request_start,
        upstream_start: Instant::now(), // will be updated below
        is_stream,
        raw_request_body: raw_request_body.clone(),
    };

    let mut req = build_provider_request(&state.client, &provider, body_json);
    if !is_stream {
        req = req.timeout(Duration::from_secs(NON_STREAM_REQUEST_TIMEOUT_SECS));
    }

    req_log.mark_upstream_start();
    let upstream_start = req_log.upstream_start;
    request_guard.set_phase("send_upstream");

    // Acquire concurrency permit if configured
    let _permit = if let Some(ref sem) = state.concurrency_semaphore {
        match sem.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => {
                warn!(
                    request_id = request_id.as_str(),
                    max_concurrent = state.config.server.max_concurrent_requests,
                    "并发请求数已达上限"
                );
                request_guard.complete();
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
            error!(
                request_id = request_id.as_str(),
                error = %e,
                upstream_total_ms = elapsed_ms(upstream_start),
                request_total_ms = elapsed_ms(request_start),
                "上游请求发送失败"
            );
            request_guard.complete();
            state.inc_failed_requests();
            req_log.emit_send_error(&e.to_string());
            return Err(AppError::Http(e));
        }
    };
    let upstream_headers_ms = elapsed_ms(upstream_start);
    let status = upstream_resp.status();
    request_guard.set_phase("received_upstream_headers");

    info!(
        request_id = request_id.as_str(),
        status = %status,
        upstream_headers_ms,
        "收到上游响应头"
    );

    if !status.is_success() {
        let text = upstream_resp.text().await.unwrap_or_default();
        let status_code = status.as_u16();

        // Check if fallback is enabled and this status code is eligible
        if state.config.fallback.enabled
            && state.config.fallback.on_status_codes.contains(&status_code)
        {
            let registry = state.registry.load();
            let current_name = provider.name.clone();

            let ctx = fallback::FallbackContext {
                client: &state.client,
                raw_body_bytes: &bytes,
                global_routes,
                request_id: &request_id,
                request_start,
                input_format: InputFormat::Anthropic,
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

        // No fallback or all fallbacks failed
        let log_body = truncate_for_log(&text, MAX_LOG_BODY_BYTES);
        error!(
            request_id = request_id.as_str(),
            status = %status,
            body_bytes = text.len(),
            body = %log_body,
            upstream_total_ms = elapsed_ms(upstream_start),
            request_total_ms = elapsed_ms(request_start),
            "上游返回错误"
        );
        state.inc_failed_requests();
        request_guard.complete();
        req_log.emit_upstream_error(status_code, upstream_headers_ms, &text);

        return Ok((
            StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(json!({
                "type": "error",
                "error": {
                    "type": "upstream_error",
                    "message": sanitize_upstream_error(status_code, &text)
                }
            })),
        )
            .into_response());
    }

    let model = provider.model.clone();

    request_guard.set_phase(if is_stream {
        "handle_stream_response"
    } else {
        "handle_non_stream_response"
    });

    // Use provider dispatch to handle the response based on format
    let dispatch = get_dispatch(&provider.format)
        .expect("Kiro requests are handled before this point");

    let stream_log_ctx = if is_stream {
        Some(req_log.to_stream_log_ctx())
    } else {
        None
    };

    let prepared = crate::server::provider_dispatch::PreparedRequest {
        body: serde_json::Value::Null, // body already sent
        is_stream,
        tool_name_map,
        provider_model: model,
    };

    let response = dispatch
        .handle_messages_response(
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
