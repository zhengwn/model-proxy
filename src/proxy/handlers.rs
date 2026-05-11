use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::HeaderMap,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use super::convert::{build_provider_request, prepare_body};
use super::passthrough::{handle_non_stream_passthrough, handle_stream_passthrough};
use super::response::handle_non_stream;
use super::state::{
    elapsed_ms, next_request_id, RequestCompletionGuard, MAX_LOG_BODY_BYTES,
    NON_STREAM_REQUEST_TIMEOUT_SECS,
};
use super::stream::handle_stream;
use super::utils::{message_count, tool_count, truncate_for_log};
use crate::{
    config::Config,
    error::{AppError, Result},
};

fn check_auth(headers: &HeaderMap, config: &Config) -> Result<()> {
    if let Some(expected_key) = &config.server.api_key {
        let provided = headers
            .get("x-api-key")
            .or_else(|| headers.get("authorization"))
            .and_then(|v| v.to_str().ok());

        let provided_clean = provided.map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim());

        if provided_clean != Some(expected_key.as_str()) {
            warn!("API key 验证失败");
            return Err(AppError::Unauthorized);
        }
    }
    Ok(())
}

pub async fn proxy_messages(
    State(state): State<super::state::AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let request_id = next_request_id();
    let request_start = Instant::now();
    let mut request_guard = RequestCompletionGuard::new(request_id.clone(), request_start);

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
                AppError::Request(format!("读取请求体失败: {}", msg))
            }
        })?;

    let body_json: Value = serde_json::from_slice(&bytes)?;
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

    request_guard.set_phase("prepare_body");
    let (body_json, is_stream, tool_name_map) = prepare_body(body_json, &state.config)?;
    let provider_model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(state.config.provider.model.as_str());
    let reasoning_effort = body_json
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let route_reasoning_effort = state
        .config
        .provider
        .resolve_route_reasoning_effort(Some(requested_model.as_str()))
        .unwrap_or("");

    info!(
        request_id = request_id.as_str(),
        requested_model = requested_model.as_str(),
        provider_model,
        route_reasoning_effort,
        reasoning_effort,
        provider_format = ?state.config.provider.format,
        stream = is_stream,
        messages = message_count(&body_json),
        tools = tool_count(&body_json),
        "准备转发上游请求"
    );

    let mut req = build_provider_request(&state.client, &state.config, body_json);
    if !is_stream {
        req = req.timeout(Duration::from_secs(NON_STREAM_REQUEST_TIMEOUT_SECS));
    }

    let upstream_start = Instant::now();
    request_guard.set_phase("send_upstream");
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
        let response = (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(json!({
                "type": "error",
                "error": {
                    "type": "upstream_error",
                    "message": text
                }
            })),
        )
            .into_response();
        request_guard.complete();
        return Ok(response);
    }

    let model = state.config.provider.model.clone();

    request_guard.set_phase(if is_stream {
        "handle_stream_response"
    } else {
        "handle_non_stream_response"
    });
    let response = match state.config.provider.format {
        crate::config::ProviderFormat::Openai => {
            if is_stream {
                handle_stream(
                    upstream_resp,
                    &model,
                    Arc::new(tool_name_map),
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            } else {
                handle_non_stream(
                    upstream_resp,
                    &model,
                    &tool_name_map,
                    &request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
        crate::config::ProviderFormat::Anthropic => {
            if is_stream {
                handle_stream_passthrough(
                    upstream_resp,
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            } else {
                handle_non_stream_passthrough(
                    upstream_resp,
                    &request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
    };
    request_guard.complete();
    response
}

pub async fn event_logging_batch(body: String) -> Json<Value> {
    let body_len = body.len();
    let body_preview = truncate_for_log(&body, MAX_LOG_BODY_BYTES);
    let event_count = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.as_array().map(|arr| arr.len()))
        .unwrap_or(0);
    debug!(
        event_count,
        body_bytes = body_len,
        body = %body_preview,
        "收到遥测事件"
    );
    Json(json!({"status": "ok"}))
}
