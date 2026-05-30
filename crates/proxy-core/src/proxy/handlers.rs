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
use subtle::ConstantTimeEq;
use tracing::{debug, error, info, warn};

use super::convert::{build_provider_request, prepare_body, prepare_chat_completions_body};
use super::fallback::{self, try_fallback, InputFormat};
use super::passthrough::{handle_non_stream_passthrough, handle_stream_passthrough};
use super::response::{handle_non_stream, handle_non_stream_openai_output};
use super::state::{
    elapsed_ms, next_request_id, RequestCompletionGuard, MAX_LOG_BODY_BYTES,
    NON_STREAM_REQUEST_TIMEOUT_SECS,
};
use super::stream::{handle_stream, handle_stream_openai_output, StreamLogContext};
use super::utils::{message_count, tool_count, truncate_for_log};
use crate::{
    config::Config,
    error::{AppError, Result},
    logging::{truncate_body, LogCollector, LogEntry},
};

// ---- Log entry builder ----

/// Context for building a log entry, capturing common fields shared across all log points.
struct LogContext<'a> {
    request_id: &'a str,
    method: &'static str,
    path: &'static str,
    provider: &'a str,
    model: &'a str,
    requested_model: &'a str,
    request_start: Instant,
    upstream_start: Instant,
    is_stream: bool,
    raw_request_body: &'a str,
}

/// Emit a log entry if the collector's filter allows it.
/// Centralizes all LogEntry construction to avoid repetition.
fn emit_log_entry(
    collector: &LogCollector,
    ctx: &LogContext<'_>,
    status: u16,
    ttft_ms: Option<u64>,
    error_message: Option<String>,
    response_body: Option<&str>,
) {
    if !collector.should_log(status) {
        return;
    }
    let log_config = collector.config.load();
    let duration_ms = ctx.request_start.elapsed().as_millis() as u64;
    let proxy_overhead_ms = ctx
        .upstream_start
        .duration_since(ctx.request_start)
        .as_millis() as u64;

    let entry = LogEntry {
        id: ctx.request_id.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        method: ctx.method.to_string(),
        path: ctx.path.to_string(),
        provider: ctx.provider.to_string(),
        model: ctx.model.to_string(),
        requested_model: Some(ctx.requested_model.to_string()),
        status,
        duration_ms,
        proxy_overhead_ms: Some(proxy_overhead_ms),
        ttft_ms,
        error_message,
        request_body: if log_config.record_body {
            Some(truncate_body(
                ctx.raw_request_body,
                log_config.max_body_bytes,
            ))
        } else {
            None
        },
        response_body: if log_config.record_body {
            response_body.map(|b| truncate_body(b, log_config.max_body_bytes))
        } else {
            None
        },
        is_stream: ctx.is_stream,
        token_count: None,
    };
    collector.emit(entry);
}

fn check_auth(headers: &HeaderMap, config: &Config) -> Result<()> {
    if let Some(expected_key) = &config.server.api_key {
        let provided = headers
            .get("x-api-key")
            .or_else(|| headers.get("authorization"))
            .and_then(|v| v.to_str().ok());

        let provided_clean = provided.map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim());

        let is_valid = match provided_clean {
            Some(key) => {
                // Use constant-time comparison to prevent timing attacks
                let key_bytes = key.as_bytes();
                let expected_bytes = expected_key.as_bytes();
                key_bytes.len() == expected_bytes.len() && key_bytes.ct_eq(expected_bytes).into()
            }
            None => false,
        };

        if !is_valid {
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

    // Track total requests
    state.inc_total_requests();

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

    // Capture the raw request body string for potential logging
    let raw_request_body = String::from_utf8_lossy(&bytes).into_owned();

    // Load the current active provider (lock-free via ArcSwap)
    let provider = state.current_provider();
    let model_routes = state.current_model_routes();

    request_guard.set_phase("prepare_body");
    let global_routes = model_routes.as_slice();
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

    let mut req = build_provider_request(&state.client, &provider, body_json);
    if !is_stream {
        req = req.timeout(Duration::from_secs(NON_STREAM_REQUEST_TIMEOUT_SECS));
    }

    let upstream_start = Instant::now();
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

            let log_ctx = LogContext {
                request_id: &request_id,
                method: "POST",
                path: "/v1/messages",
                provider: &log_provider_name,
                model: &log_model,
                requested_model: &requested_model,
                request_start,
                upstream_start,
                is_stream,
                raw_request_body: &raw_request_body,
            };
            emit_log_entry(
                &state.log_collector,
                &log_ctx,
                502,
                None,
                Some(e.to_string()),
                None,
            );

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
                upstream_start,
                upstream_headers_ms,
                input_format: InputFormat::Anthropic,
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
        let response = (
            StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY),
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

        let log_ctx = LogContext {
            request_id: &request_id,
            method: "POST",
            path: "/v1/messages",
            provider: &log_provider_name,
            model: &log_model,
            requested_model: &requested_model,
            request_start,
            upstream_start,
            is_stream,
            raw_request_body: &raw_request_body,
        };
        emit_log_entry(
            &state.log_collector,
            &log_ctx,
            status_code,
            Some(upstream_headers_ms as u64),
            Some(truncate_for_log(&text, MAX_LOG_BODY_BYTES)),
            Some(&text),
        );

        return Ok(response);
    }

    let model = provider.model.clone();

    request_guard.set_phase(if is_stream {
        "handle_stream_response"
    } else {
        "handle_non_stream_response"
    });
    let response = match provider.format {
        crate::config::ProviderFormat::Openai => {
            if is_stream {
                handle_stream(
                    upstream_resp,
                    &model,
                    Arc::new(tool_name_map),
                    request_id.clone(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    Some(StreamLogContext {
                        collector: state.log_collector.clone(),
                        request_id: request_id.clone(),
                        method: "POST",
                        path: "/v1/messages",
                        provider: log_provider_name.clone(),
                        model: log_model.clone(),
                        requested_model: requested_model.clone(),
                        request_start,
                        upstream_start,
                        raw_request_body: raw_request_body.clone(),
                    }),
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
                    request_id.clone(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    Some(StreamLogContext {
                        collector: state.log_collector.clone(),
                        request_id: request_id.clone(),
                        method: "POST",
                        path: "/v1/messages",
                        provider: log_provider_name.clone(),
                        model: log_model.clone(),
                        requested_model: requested_model.clone(),
                        request_start,
                        upstream_start,
                        raw_request_body: raw_request_body.clone(),
                    }),
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

    if !is_stream {
        let log_ctx = LogContext {
            request_id: &request_id,
            method: "POST",
            path: "/v1/messages",
            provider: &log_provider_name,
            model: &log_model,
            requested_model: &requested_model,
            request_start,
            upstream_start,
            is_stream,
            raw_request_body: &raw_request_body,
        };
        emit_log_entry(
            &state.log_collector,
            &log_ctx,
            200,
            Some(upstream_headers_ms as u64),
            None,
            None,
        );
    }

    response
}

/// OpenAI-compatible endpoint: accepts OpenAI Chat Completions format requests.
/// Forwards directly to OpenAI-format providers, or returns an error for Anthropic-format providers.
pub async fn proxy_chat_completions(
    State(state): State<super::state::AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let request_id = next_request_id();
    let request_start = Instant::now();

    state.inc_total_requests();

    check_auth(&headers, &state.config)?;

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

    let mut body_json: Value = serde_json::from_slice(&bytes)?;
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
    let global_routes = model_routes.as_slice();
    let provider_model = provider
        .resolve_model_with_routes(Some(requested_model.as_str()), global_routes)
        .to_string();
    if let Some(obj) = body_json.as_object_mut() {
        obj.insert("model".to_string(), json!(provider_model));
    }

    // Capture for logging
    let raw_request_body = String::from_utf8_lossy(&bytes).into_owned();
    let log_provider_name = provider.name.clone();
    let log_model = provider_model.clone();

    info!(
        request_id = request_id.as_str(),
        requested_model = requested_model.as_str(),
        provider_model = provider_model.as_str(),
        provider_format = ?provider.format,
        stream = is_stream,
        "收到 OpenAI 格式请求 (/v1/chat/completions)"
    );

    let upstream_start = Instant::now();

    match provider.format {
        crate::config::ProviderFormat::Openai => {
            // Direct forward to OpenAI-format provider
            let mut req = build_provider_request(&state.client, &provider, body_json);
            if !is_stream {
                req = req.timeout(Duration::from_secs(NON_STREAM_REQUEST_TIMEOUT_SECS));
            }

            // Acquire concurrency permit if configured
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
                    let log_ctx = LogContext {
                        request_id: &request_id,
                        method: "POST",
                        path: "/v1/chat/completions",
                        provider: &log_provider_name,
                        model: &log_model,
                        requested_model: &requested_model,
                        request_start,
                        upstream_start,
                        is_stream,
                        raw_request_body: &raw_request_body,
                    };
                    emit_log_entry(
                        &state.log_collector,
                        &log_ctx,
                        502,
                        None,
                        Some(e.to_string()),
                        None,
                    );
                    return Err(AppError::Http(e));
                }
            };

            let status = upstream_resp.status();
            if !status.is_success() {
                let text = upstream_resp.text().await.unwrap_or_default();
                let status_code = status.as_u16();
                state.inc_failed_requests();

                let upstream_headers_ms = elapsed_ms(upstream_start);
                let log_ctx = LogContext {
                    request_id: &request_id,
                    method: "POST",
                    path: "/v1/chat/completions",
                    provider: &log_provider_name,
                    model: &log_model,
                    requested_model: &requested_model,
                    request_start,
                    upstream_start,
                    is_stream,
                    raw_request_body: &raw_request_body,
                };
                emit_log_entry(
                    &state.log_collector,
                    &log_ctx,
                    status_code,
                    Some(upstream_headers_ms as u64),
                    Some(truncate_for_log(&text, MAX_LOG_BODY_BYTES)),
                    Some(&text),
                );

                return Ok((
                    StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(json!({
                        "error": {
                            "message": text,
                            "type": "upstream_error"
                        }
                    })),
                )
                    .into_response());
            }

            let upstream_headers_ms = elapsed_ms(upstream_start);

            // Pass through the response as-is (already in OpenAI format)
            let response = if is_stream {
                handle_stream_passthrough(
                    upstream_resp,
                    request_id.clone(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    Some(StreamLogContext {
                        collector: state.log_collector.clone(),
                        request_id: request_id.clone(),
                        method: "POST",
                        path: "/v1/chat/completions",
                        provider: log_provider_name.clone(),
                        model: log_model.clone(),
                        requested_model: requested_model.clone(),
                        request_start,
                        upstream_start,
                        raw_request_body: raw_request_body.clone(),
                    }),
                )
                .await
            } else {
                let body_bytes = upstream_resp.bytes().await?;
                info!(
                    request_id = request_id.as_str(),
                    body_bytes = body_bytes.len(),
                    request_total_ms = elapsed_ms(request_start),
                    "OpenAI 格式非流式响应完成"
                );
                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(body_bytes))
                    .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))?)
            };

            if !is_stream {
                let log_ctx = LogContext {
                    request_id: &request_id,
                    method: "POST",
                    path: "/v1/chat/completions",
                    provider: &log_provider_name,
                    model: &log_model,
                    requested_model: &requested_model,
                    request_start,
                    upstream_start,
                    is_stream,
                    raw_request_body: &raw_request_body,
                };
                emit_log_entry(
                    &state.log_collector,
                    &log_ctx,
                    200,
                    Some(upstream_headers_ms as u64),
                    None,
                    None,
                );
            }

            response
        }
        crate::config::ProviderFormat::Anthropic => {
            // Convert OpenAI request to Anthropic format
            let (body_json, is_stream, _tool_name_map) =
                prepare_chat_completions_body(body_json, &provider, global_routes)?;

            let provider_model = body_json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or(provider.model.as_str())
                .to_string();
            let log_model = provider_model.clone();

            let mut req = build_provider_request(&state.client, &provider, body_json);
            if !is_stream {
                req = req.timeout(Duration::from_secs(NON_STREAM_REQUEST_TIMEOUT_SECS));
            }

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
                    let log_ctx = LogContext {
                        request_id: &request_id,
                        method: "POST",
                        path: "/v1/chat/completions",
                        provider: &log_provider_name,
                        model: &log_model,
                        requested_model: &requested_model,
                        request_start,
                        upstream_start,
                        is_stream,
                        raw_request_body: &raw_request_body,
                    };
                    emit_log_entry(
                        &state.log_collector,
                        &log_ctx,
                        502,
                        None,
                        Some(e.to_string()),
                        None,
                    );
                    return Err(AppError::Http(e));
                }
            };

            let status = upstream_resp.status();
            let upstream_headers_ms = elapsed_ms(upstream_start);
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
                        raw_body_bytes: &raw_request_body.as_bytes(),
                        global_routes,
                        request_id: &request_id,
                        request_start,
                        upstream_start,
                        upstream_headers_ms,
                        input_format: InputFormat::OpenAI,
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
                        return response;
                    }
                }

                state.inc_failed_requests();
                let upstream_headers_ms = elapsed_ms(upstream_start);
                let log_ctx = LogContext {
                    request_id: &request_id,
                    method: "POST",
                    path: "/v1/chat/completions",
                    provider: &log_provider_name,
                    model: &log_model,
                    requested_model: &requested_model,
                    request_start,
                    upstream_start,
                    is_stream,
                    raw_request_body: &raw_request_body,
                };
                emit_log_entry(
                    &state.log_collector,
                    &log_ctx,
                    status_code,
                    Some(upstream_headers_ms as u64),
                    Some(truncate_for_log(&text, MAX_LOG_BODY_BYTES)),
                    Some(&text),
                );

                return Ok((
                    StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(json!({
                        "error": {
                            "message": text,
                            "type": "upstream_error"
                        }
                    })),
                )
                    .into_response());
            }

            let upstream_headers_ms = elapsed_ms(upstream_start);

            // Convert Anthropic response to OpenAI format
            let response = if is_stream {
                handle_stream_openai_output(
                    upstream_resp,
                    &log_model,
                    request_id.clone(),
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                    Some(StreamLogContext {
                        collector: state.log_collector.clone(),
                        request_id: request_id.clone(),
                        method: "POST",
                        path: "/v1/chat/completions",
                        provider: log_provider_name.clone(),
                        model: log_model.clone(),
                        requested_model: requested_model.clone(),
                        request_start,
                        upstream_start,
                        raw_request_body: raw_request_body.clone(),
                    }),
                )
                .await
            } else {
                handle_non_stream_openai_output(
                    upstream_resp,
                    &log_model,
                    &request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            };

            if !is_stream {
                let log_ctx = LogContext {
                    request_id: &request_id,
                    method: "POST",
                    path: "/v1/chat/completions",
                    provider: &log_provider_name,
                    model: &log_model,
                    requested_model: &requested_model,
                    request_start,
                    upstream_start,
                    is_stream,
                    raw_request_body: &raw_request_body,
                };
                emit_log_entry(
                    &state.log_collector,
                    &log_ctx,
                    200,
                    Some(upstream_headers_ms as u64),
                    None,
                    None,
                );
            }

            response
        }
    }
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
