use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, Request, State},
    http::{header, HeaderMap},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tracing::{debug, error, info, warn};

use crate::convert::anthropic_openai::request::{build_provider_request, prepare_body, prepare_chat_completions_body};
use crate::convert::anthropic_openai::response::{handle_non_stream, handle_non_stream_openai_output};
use crate::convert::anthropic_openai::stream::{handle_stream, handle_stream_openai_output, StreamLogContext};
use crate::convert::kiro::auth::KiroAuthManager;
use crate::convert::passthrough::{handle_non_stream_passthrough, handle_stream_passthrough};
use crate::convert::utils::{message_count, tool_count, truncate_for_log};
use super::fallback::{self, try_fallback, InputFormat};
use super::state::{
    elapsed_ms, next_request_id, RequestCompletionGuard, MAX_LOG_BODY_BYTES,
    NON_STREAM_REQUEST_TIMEOUT_SECS,
};
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

/// Sanitize upstream error messages before forwarding to clients.
/// - 4xx errors: forward truncated message (client can fix their request)
/// - 5xx errors: generic message (don't leak internal infrastructure details)
fn sanitize_upstream_error(status: u16, body: &str) -> String {
    if (400..500).contains(&status) {
        truncate_for_log(body, 512)
    } else {
        format!("Upstream service error (HTTP {})", status)
    }
}

/// Multi-tenant auth result.
pub enum AuthResult {
    /// Global API key matched, or no API key configured
    GlobalOk,
    /// Multi-tenant: extracted per-request refresh token
    TenantOk { refresh_token: String },
    /// Authentication failed
    Unauthorized,
}

/// Check authentication, supporting both global API key and multi-tenant
/// `API_KEY:REFRESH_TOKEN` format.
pub fn check_auth_tenant(headers: &HeaderMap, config: &Config) -> AuthResult {
    let Some(expected_key) = &config.server.api_key else {
        return AuthResult::GlobalOk;
    };

    let provided = headers
        .get("x-api-key")
        .or_else(|| headers.get("authorization"))
        .and_then(|v| v.to_str().ok());

    let provided_clean = provided.map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim());

    match provided_clean {
        Some(key) => {
            // Check for multi-tenant format: API_KEY:REFRESH_TOKEN
            if let Some((api_key_part, refresh_token)) = key.split_once(':') {
                if !refresh_token.is_empty() {
                    let key_bytes = api_key_part.as_bytes();
                    let expected_bytes = expected_key.as_bytes();
                    if key_bytes.len() == expected_bytes.len()
                        && key_bytes.ct_eq(expected_bytes).into()
                    {
                        return AuthResult::TenantOk {
                            refresh_token: refresh_token.to_string(),
                        };
                    }
                }
            }

            // Standard single-key check
            let key_bytes = key.as_bytes();
            let expected_bytes = expected_key.as_bytes();
            if key_bytes.len() == expected_bytes.len() && key_bytes.ct_eq(expected_bytes).into() {
                AuthResult::GlobalOk
            } else {
                warn!("API key 验证失败");
                AuthResult::Unauthorized
            }
        }
        None => {
            warn!("API key 验证失败");
            AuthResult::Unauthorized
        }
    }
}

fn check_auth(headers: &HeaderMap, config: &Config) -> Result<()> {
    match check_auth_tenant(headers, config) {
        AuthResult::GlobalOk | AuthResult::TenantOk { .. } => Ok(()),
        AuthResult::Unauthorized => Err(AppError::Unauthorized),
    }
}

/// Axum middleware that enforces client API key authentication.
/// Skip auth for public endpoints: /health, /metrics, /v1/models.
pub async fn client_auth_middleware(
    State(state): State<super::state::AppState>,
    headers: HeaderMap,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let is_public = matches!(path, "/health" | "/metrics" | "/v1/models");
    if !is_public {
        if let Err(e) = check_auth(&headers, &state.config) {
            return e.into_response();
        }
    }
    next.run(req).await
}

pub async fn proxy_messages(
    State(state): State<super::state::AppState>,
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
    if provider.format == crate::config::ProviderFormat::Kiro {
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
                    "message": sanitize_upstream_error(status_code, &text)
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
        crate::config::ProviderFormat::Kiro => {
            // Kiro is handled by early return above — this is unreachable
            unreachable!("Kiro requests are handled before this match")
        }
    };

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

    // Record metrics at handler exit
    if let Some(ref m) = state.metrics {
        match &response {
            Ok(resp) => {
                if resp.status().is_server_error() {
                    m.record_error(request_start.elapsed().as_millis() as u64);
                } else {
                    m.record_request(request_start.elapsed().as_millis() as u64);
                }
            }
            Err(_) => {
                m.record_error(request_start.elapsed().as_millis() as u64);
            }
        }
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
    let mut request_guard = RequestCompletionGuard::new(request_id.clone(), request_start);

    state.inc_total_requests();
    if let Some(ref metrics) = state.metrics {
        metrics.connection_start();
        request_guard.set_metrics(metrics.clone());
    }

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

    // Kiro provider: convert OpenAI → Kiro → Anthropic → OpenAI
    if provider.format == crate::config::ProviderFormat::Kiro {
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
                            "message": sanitize_upstream_error(status_code, &text),
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

            // Record metrics at handler exit
            if let Some(ref m) = state.metrics {
                match &response {
                    Ok(resp) => {
                        if resp.status().is_server_error() {
                            m.record_error(request_start.elapsed().as_millis() as u64);
                        } else {
                            m.record_request(request_start.elapsed().as_millis() as u64);
                        }
                    }
                    Err(_) => {
                        m.record_error(request_start.elapsed().as_millis() as u64);
                    }
                }
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
                            "message": sanitize_upstream_error(status_code, &text),
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

            // Record metrics at handler exit
            if let Some(ref m) = state.metrics {
                match &response {
                    Ok(resp) => {
                        if resp.status().is_server_error() {
                            m.record_error(request_start.elapsed().as_millis() as u64);
                        } else {
                            m.record_request(request_start.elapsed().as_millis() as u64);
                        }
                    }
                    Err(_) => {
                        m.record_error(request_start.elapsed().as_millis() as u64);
                    }
                }
            }
            response
        }
        crate::config::ProviderFormat::Kiro => {
            // Kiro is handled by early return above — this is unreachable
            unreachable!("Kiro requests are handled before this match")
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

// ---- Kiro shared dispatch ----

/// Auth info obtained from Kiro credential management.
struct KiroAuthInfo {
    token: String,
    amz_user_agent: String,
    user_agent: String,
    is_account_managed: bool,
    acct_idx: usize,
    profile_arn: Option<String>,
}

/// Result of a successful Kiro upstream dispatch.
struct KiroDispatchResult {
    response: reqwest::Response,
    acct_idx: usize,
    is_account_managed: bool,
    upstream_headers_ms: u128,
    endpoint_name: String,
}

/// Acquire auth info from the Kiro credential manager.
async fn acquire_kiro_auth(
    state: &super::state::AppState,
    kiro_config: &crate::config::KiroConfig,
    tenant_refresh_token: Option<&str>,
) -> Result<KiroAuthInfo> {
    if let Some(tenant_token) = tenant_refresh_token {
        let mut tenant_cfg = kiro_config.clone();
        tenant_cfg.auth_method = "social".to_string();
        tenant_cfg.refresh_token = Some(tenant_token.to_string());
        let mut auth = KiroAuthManager::new(&tenant_cfg, state.client.clone());
        let token = auth.get_valid_token().await?;
        return Ok(KiroAuthInfo {
            token,
            amz_user_agent: auth.amz_user_agent(),
            user_agent: auth.user_agent(),
            is_account_managed: false,
            acct_idx: 0,
            profile_arn: auth.profile_arn().map(|s| s.to_string()),
        });
    }

    if let Some(ref account_mgr) = state.kiro_account_manager {
        let mut mgr = account_mgr.lock().await;
        let (acct_idx, _id, auth_arc_ref) = match mgr.get_available_account(&[]) {
            Some(triple) => triple,
            None => {
                if mgr.self_heal() {
                    mgr.get_available_account(&[]).ok_or_else(|| {
                        AppError::Request("所有 Kiro 账户不可用 (已尝试自愈)".to_string())
                    })?
                } else {
                    return Err(AppError::Request("所有 Kiro 账户不可用".to_string()));
                }
            }
        };
        let auth_arc = auth_arc_ref.clone();
        let acct_idx = acct_idx;
        drop(mgr);
        let mut auth = auth_arc.lock().await;
        let token = auth.get_valid_token().await?;
        let profile_arn = auth.profile_arn().map(|s| s.to_string());
        return Ok(KiroAuthInfo {
            token,
            amz_user_agent: auth.amz_user_agent(),
            user_agent: auth.user_agent(),
            is_account_managed: true,
            acct_idx,
            profile_arn,
        });
    }

    if let Some(ref auth_arc) = state.kiro_auth {
        let mut auth = auth_arc.lock().await;
        let token = auth.get_valid_token().await?;
        return Ok(KiroAuthInfo {
            token,
            amz_user_agent: auth.amz_user_agent(),
            user_agent: auth.user_agent(),
            is_account_managed: false,
            acct_idx: 0,
            profile_arn: auth.profile_arn().map(|s| s.to_string()),
        });
    }

    let mut auth = KiroAuthManager::new(kiro_config, state.client.clone());
    let token = auth.get_valid_token().await?;
    Ok(KiroAuthInfo {
        token,
        amz_user_agent: auth.amz_user_agent(),
        user_agent: auth.user_agent(),
        is_account_managed: false,
        acct_idx: 0,
        profile_arn: auth.profile_arn().map(|s| s.to_string()),
    })
}

/// Dispatch a Kiro payload to the upstream API with multi-endpoint fallback.
///
/// Tries each endpoint in priority order. On 429, records failure and tries next.
/// On 401/403/402, returns immediately (no cross-endpoint retry).
async fn dispatch_kiro_request(
    state: &super::state::AppState,
    kiro_payload: &Value,
    payload_bytes: &[u8],
    auth_info: &KiroAuthInfo,
    kiro_config: &crate::config::KiroConfig,
    request_id: &str,
    is_stream: bool,
) -> Result<KiroDispatchResult> {
    use crate::convert::kiro::endpoint::{
        build_endpoint_headers, build_endpoint_url, get_sorted_endpoints, PreferredEndpoint,
    };

    let region = kiro_config
        .api_region
        .as_deref()
        .unwrap_or(&kiro_config.region);

    let preferred = PreferredEndpoint::from_str_opt(kiro_config.preferred_endpoint.as_deref());
    let fallback = kiro_config.endpoint_fallback.unwrap_or(true);
    let endpoints = get_sorted_endpoints(preferred, fallback);

    // Track inflight for account manager
    if auth_info.is_account_managed {
        if let Some(ref mgr) = state.kiro_account_manager {
            mgr.lock().await.increment_inflight_at(auth_info.acct_idx);
        }
    }

    // Acquire concurrency permit
    let _permit = if let Some(ref sem) = state.concurrency_semaphore {
        match sem.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => {
                state.inc_failed_requests();
                if auth_info.is_account_managed {
                    if let Some(ref mgr) = state.kiro_account_manager {
                        mgr.lock().await.release_inflight_at(auth_info.acct_idx);
                    }
                }
                return Err(AppError::TooManyRequests);
            }
        }
    } else {
        None
    };

    // Rate limiter check
    if let Some(ref rl) = state.rate_limiter {
        let mut limiter = rl.lock().await;
        if let Err(wait) = limiter.check("kiro") {
            warn!(wait_ms = wait.as_millis(), "Kiro 请求被限流，等待");
            tokio::time::sleep(wait).await;
        }
    }

    let timeout = if !is_stream {
        Some(Duration::from_secs(super::state::NON_STREAM_REQUEST_TIMEOUT_SECS))
    } else {
        None
    };

    // Build proxy-aware client
    let proxy_url = state.kiro_account_manager.as_ref().and_then(|m| {
        m.try_lock()
            .ok()
            .and_then(|mgr| mgr.current_proxy_url().map(|s| s.to_string()))
    });
    let kiro_client = build_kiro_client(proxy_url.as_deref());

    let mut last_error: Option<String> = None;

    for endpoint in &endpoints {
        let url = build_endpoint_url(endpoint, region);
        let headers =
            build_endpoint_headers(endpoint, &auth_info.token, &auth_info.amz_user_agent, &auth_info.user_agent);

        let upstream_start = Instant::now();

        let upstream_resp = match kiro_request_with_retry(
            &kiro_client,
            &url,
            payload_bytes,
            &headers,
            state.kiro_auth.as_ref(),
            request_id,
            timeout,
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) => {
                last_error = Some(e.to_string());
                if let Some(ref tracker) = state.endpoint_health {
                    tracker.record_failure(endpoint.name);
                }
                continue;
            }
        };

        let status = upstream_resp.status();
        let status_code = status.as_u16();

        // 429: try next endpoint (unless this is the last one)
        if status_code == 429 {
            if let Some(ref tracker) = state.endpoint_health {
                tracker.record_failure(endpoint.name);
            }
            // Also set cooldown on the account if managed
            if auth_info.is_account_managed {
                if let Some(ref mgr) = state.kiro_account_manager {
                    let mut mgr = mgr.lock().await;
                    if let Some(acct_id) = mgr.account_id_at(auth_info.acct_idx) {
                        mgr.set_cooldown_for_account(&acct_id);
                    }
                }
            }
            warn!(
                request_id,
                endpoint = endpoint.name,
                status = status_code,
                "Kiro endpoint 返回 429，尝试下一个"
            );
            last_error = Some(format!("429 from {}", endpoint.name));
            continue;
        }

        // 401/403/402: do NOT try next endpoint
        if status_code == 401 || status_code == 403 || status_code == 402 {
            if let Some(ref tracker) = state.endpoint_health {
                tracker.record_failure(endpoint.name);
            }
        }

        // Release inflight tracking
        if auth_info.is_account_managed {
            if let Some(ref mgr) = state.kiro_account_manager {
                mgr.lock().await.release_inflight_with_latency_at(
                    auth_info.acct_idx,
                    upstream_start.elapsed().as_millis() as f64,
                );
            }
        }

        let upstream_headers_ms = elapsed_ms(upstream_start);

        // Record success for circuit breaker
        if status.is_success() {
            if auth_info.is_account_managed {
                if let Some(ref mgr) = state.kiro_account_manager {
                    mgr.lock().await.record_success_at(auth_info.acct_idx);
                }
            }
            if let Some(ref tracker) = state.endpoint_health {
                tracker.record_success(endpoint.name, upstream_headers_ms as f64);
            }
        } else {
            // Record failure for circuit breaker
            let err_body = upstream_resp.text().await.unwrap_or_default();
            let error_class =
                crate::convert::kiro::account::classify_error(status_code, &err_body);
            if auth_info.is_account_managed {
                if let Some(ref mgr_arc) = state.kiro_account_manager {
                    let mut mgr = mgr_arc.lock().await;
                    match error_class {
                        crate::convert::kiro::account::ErrorClass::Suspended => {
                            if let Some(acct_id) = mgr.account_id_at(auth_info.acct_idx) {
                                warn!(
                                    account_id = acct_id.as_str(),
                                    "账户已自动禁用 (temporarily_suspended)"
                                );
                                mgr.set_disabled(&acct_id, true);
                            }
                        }
                        crate::convert::kiro::account::ErrorClass::Recoverable => {
                            mgr.record_failure_at(auth_info.acct_idx, error_class.clone());
                            if status_code == 429 {
                                if let Some(acct_id) = mgr.account_id_at(auth_info.acct_idx) {
                                    mgr.set_cooldown_for_account(&acct_id);
                                }
                            }
                        }
                        crate::convert::kiro::account::ErrorClass::Fatal => {}
                    }
                }
            }
            error!(
                request_id,
                endpoint = endpoint.name,
                status = status_code,
                body = err_body.as_str(),
                "Kiro 上游返回错误"
            );
            // Return error response directly (not as Err) for upstream errors
            return Err(AppError::UpstreamStatus(status_code, err_body));
        }

        return Ok(KiroDispatchResult {
            response: upstream_resp,
            acct_idx: auth_info.acct_idx,
            is_account_managed: auth_info.is_account_managed,
            upstream_headers_ms,
            endpoint_name: endpoint.name.to_string(),
        });
    }

    // All endpoints exhausted
    Err(AppError::Request(
        last_error.unwrap_or_else(|| "所有 Kiro endpoint 均不可用".to_string()),
    ))
}

/// Build a reqwest client with optional proxy support.
fn build_kiro_client(proxy_url: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .tcp_nodelay(true);

    if let Some(proxy_str) = proxy_url {
        if let Ok(proxy) = reqwest::Proxy::all(proxy_str) {
            builder = builder.proxy(proxy);
        }
    }

    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Send a request to the Kiro API with retry logic.
/// - 403: force_refresh token + retry once
/// - 429/5xx: exponential backoff (1s × 2^attempt), max 3 retries
async fn kiro_request_with_retry(
    client: &reqwest::Client,
    url: &str,
    payload: &[u8],
    headers: &[(String, String)],
    auth: Option<&Arc<tokio::sync::Mutex<KiroAuthManager>>>,
    request_id: &str,
    timeout: Option<Duration>,
) -> Result<reqwest::Response> {
    const MAX_RETRIES: u32 = 3;
    const BASE_DELAY_SECS: f64 = 1.0;

    let mut last_resp: Option<reqwest::Response> = None;

    for attempt in 0..=MAX_RETRIES {
        // Build request
        let mut req = client.post(url);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req = req.body(payload.to_vec());
        if let Some(t) = timeout {
            req = req.timeout(t);
        }

        // Send
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if attempt < MAX_RETRIES {
                    let delay = BASE_DELAY_SECS * 2.0_f64.powi(attempt as i32);
                    warn!(
                        request_id,
                        attempt,
                        error = %e,
                        delay_secs = delay,
                        "Kiro 请求发送失败，重试中"
                    );
                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                    continue;
                }
                return Err(AppError::Http(e));
            }
        };

        let status = resp.status().as_u16();

        // 403: force refresh token and retry once
        if status == 403 {
            if let Some(auth_arc) = auth {
                if attempt == 0 {
                    warn!(request_id, "Kiro 返回 403，强制刷新 token");
                    if let Ok(mut auth_guard) = auth_arc.try_lock() {
                        let _ = auth_guard.force_refresh().await;
                    }
                    last_resp = Some(resp);
                    continue;
                }
            }
            last_resp = Some(resp);
            break;
        }

        // 429/5xx: exponential backoff
        if status == 429 || (500..600).contains(&status) {
            if attempt < MAX_RETRIES {
                let delay = BASE_DELAY_SECS * 2.0_f64.powi(attempt as i32);
                warn!(
                    request_id,
                    status,
                    attempt,
                    delay_secs = delay,
                    "Kiro 返回 {}，重试中",
                    status
                );
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                last_resp = Some(resp);
                continue;
            }
            last_resp = Some(resp);
            break;
        }

        // Success or non-retryable error: return immediately
        return Ok(resp);
    }

    // Exhausted retries — return last response
    last_resp.ok_or_else(|| AppError::Request("Kiro 重试耗尽且无响应".to_string()))
}

/// Handle `/v1/messages` request with Kiro as upstream provider.
/// This is a separate function because Kiro uses a completely different
/// request/response protocol (AWS EventStream instead of SSE).
async fn handle_kiro_messages(
    state: super::state::AppState,
    body_json: Value,
    _raw_bytes: bytes::Bytes,
    requested_model: String,
    raw_request_body: String,
    request_id: String,
    mut request_guard: RequestCompletionGuard,
    request_start: Instant,
    tenant_refresh_token: Option<String>,
    buffered: bool,
) -> Result<Response> {
    use crate::convert::kiro::request::anthropic_to_kiro;
    use crate::convert::kiro::stream::{handle_stream_anthropic_output, handle_stream_anthropic_output_buffered};

    let provider = state.current_provider();
    let model_routes = state.current_model_routes();
    let global_routes = model_routes.as_slice();

    request_guard.set_phase("kiro_convert");

    // Convert Anthropic request to Kiro payload
    let (mut kiro_payload, tool_name_map) = anthropic_to_kiro(&body_json, &provider, global_routes)?;

    let kiro_model = kiro_payload
        .pointer("/conversationState/currentMessage/userInputMessage/modelId")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let is_stream = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    info!(
        request_id = request_id.as_str(),
        kiro_model,
        stream = is_stream,
        "Kiro 请求转换完成"
    );

    // Acquire auth
    request_guard.set_phase("kiro_auth");
    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;
    let auth_info = acquire_kiro_auth(&state, kiro_config, tenant_refresh_token.as_deref()).await?;

    // Inject profileArn into payload if available (matches kiro.rs reference)
    if let Some(ref arn) = auth_info.profile_arn {
        if let Some(obj) = kiro_payload.as_object_mut() {
            obj.insert("profileArn".to_string(), json!(arn));
        }
    }

    // Serialize payload
    request_guard.set_phase("kiro_request");
    let payload_bytes = serde_json::to_vec(&kiro_payload)?;

    // Dispatch to Kiro with endpoint fallback
    request_guard.set_phase("kiro_send");
    let dispatch_result = match dispatch_kiro_request(
        &state,
        &kiro_payload,
        &payload_bytes,
        &auth_info,
        kiro_config,
        &request_id,
        is_stream,
    )
    .await
    {
        Ok(r) => r,
        Err(AppError::UpstreamStatus(status, body)) => {
            request_guard.complete();
            return Ok((
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({
                    "type": "error",
                    "error": {
                        "type": "upstream_error",
                        "message": body
                    }
                })),
            )
                .into_response());
        }
        Err(e) => {
            request_guard.complete();
            return Err(e);
        }
    };

    // Handle response
    request_guard.set_phase("kiro_response");

    if is_stream {
        let stream_log_ctx = Some(StreamLogContext {
            collector: state.log_collector.clone(),
            request_id: request_id.clone(),
            method: "POST",
            path: if buffered { "/cc/v1/messages" } else { "/v1/messages" },
            provider: provider.name.clone(),
            model: kiro_model.to_string(),
            requested_model: requested_model.clone(),
            request_start,
            upstream_start: Instant::now(),
            raw_request_body: raw_request_body.clone(),
        });
        let first_token_timeout = std::time::Duration::from_secs(kiro_config.first_token_timeout.unwrap_or(15));
        let streaming_read_timeout = std::time::Duration::from_secs(kiro_config.streaming_read_timeout.unwrap_or(300));
        let thinking_mode = kiro_config.thinking_mode.as_deref();

        let response = if buffered {
            handle_stream_anthropic_output_buffered(
                dispatch_result.response,
                &kiro_model,
                tool_name_map,
                request_id.clone(),
                request_start,
                Instant::now(),
                dispatch_result.upstream_headers_ms,
                stream_log_ctx,
                thinking_mode,
                first_token_timeout,
                streaming_read_timeout,
            ).await
        } else {
            handle_stream_anthropic_output(
                dispatch_result.response,
                &kiro_model,
                tool_name_map,
                request_id.clone(),
                request_start,
                Instant::now(),
                dispatch_result.upstream_headers_ms,
                stream_log_ctx,
                thinking_mode,
                first_token_timeout,
                streaming_read_timeout,
            ).await
        };
        request_guard.complete();
        response
    } else {
        // Non-streaming: collect all events and build Anthropic response
        let response = handle_kiro_non_stream(
            dispatch_result.response,
            &kiro_model,
            &tool_name_map,
            &request_id,
            request_start,
            Instant::now(),
            dispatch_result.upstream_headers_ms,
        )
        .await;
        request_guard.complete();
        response
    }
}

/// Handle non-streaming Kiro response → Anthropic Messages response.
async fn handle_kiro_non_stream(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_map: &HashMap<String, String>,
    request_id: &str,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
) -> Result<Response> {
    use crate::convert::kiro::eventstream::{Event, EventStreamDecoder};

    let body_bytes = upstream_resp.bytes().await?;
    let mut decoder = EventStreamDecoder::new();
    decoder
        .feed(&body_bytes)
        .map_err(|e| AppError::Request(format!("EventStream 解析错误: {}", e)))?;

    // Collect all events
    let mut text_parts = Vec::new();
    let mut thinking_parts = Vec::new();
    let mut tool_uses = Vec::new();
    let mut input_tokens = 0u64;
    let mut output_tokens: u64;

    loop {
        match decoder.decode() {
            Ok(Some(frame)) => {
                if let Ok(event) = Event::from_frame(&frame) {
                    match event {
                        Event::AssistantResponse { content } => {
                            text_parts.push(content);
                        }
                        Event::ReasoningContent { text } => {
                            thinking_parts.push(text);
                        }
                        Event::ToolUse {
                            name,
                            tool_use_id,
                            input,
                            stop,
                        } => {
                            if stop {
                                let original_name = tool_name_map
                                    .get(name.as_str())
                                    .map(|s| s.as_str())
                                    .unwrap_or(name.as_str());
                                let input_val: Value =
                                    serde_json::from_str(&input).unwrap_or(json!({}));
                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use_id,
                                    "name": original_name,
                                    "input": input_val
                                }));
                            }
                        }
                        Event::ContextUsage { percentage } => {
                            let window =
                                crate::convert::kiro::model_map::context_window_size(model);
                            input_tokens = (percentage * window as f64 / 100.0) as u64;
                        }
                        _ => {}
                    }
                }
            }
            Ok(None) => break,
            Err(_) => {
                if decoder.is_stopped() {
                    break;
                }
            }
        }
    }

    // Build Anthropic response
    let mut content_blocks = Vec::new();

    let thinking_text = thinking_parts.join("");
    if !thinking_text.is_empty() {
        content_blocks.push(json!({
            "type": "thinking",
            "thinking": thinking_text,
            "signature": ""
        }));
    }

    let visible_text = text_parts.join("");
    if !visible_text.is_empty() {
        content_blocks.push(json!({"type": "text", "text": visible_text}));
    }

    for tu in &tool_uses {
        content_blocks.push(tu.clone());
    }

    let has_tool_use = !tool_uses.is_empty();
    output_tokens = crate::convert::kiro::stream::estimate_tokens(&visible_text) as u64;

    let stop_reason = if has_tool_use { "tool_use" } else { "end_turn" };

    let response = json!({
        "id": format!("msg_{}", next_request_id()),
        "type": "message",
        "role": "assistant",
        "content": content_blocks,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    });

    info!(
        request_id,
        has_tool_use,
        input_tokens,
        output_tokens,
        upstream_headers_ms,
        upstream_total_ms = elapsed_ms(upstream_start),
        request_total_ms = elapsed_ms(request_start),
        "Kiro 非流式响应完成"
    );

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))?)
}

/// Handle `/v1/chat/completions` with Kiro as upstream.
/// Flow: OpenAI request → Anthropic format → Kiro payload → Kiro API → response → OpenAI format
async fn handle_kiro_chat_completions(
    state: super::state::AppState,
    body_json: Value,
    requested_model: String,
    request_id: String,
    request_start: Instant,
    tenant_refresh_token: Option<String>,
) -> Result<Response> {
    use crate::convert::anthropic_openai::request::openai_to_anthropic;
    use crate::convert::kiro::request::anthropic_to_kiro;
    use crate::convert::kiro::stream::handle_stream_openai_output;

    let provider = state.current_provider();
    let model_routes = state.current_model_routes();
    let global_routes = model_routes.as_slice();

    let is_stream = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Step 1: OpenAI → Anthropic format
    let anthropic_body = openai_to_anthropic(body_json, &provider, global_routes);

    // Step 2: Anthropic → Kiro payload
    let (mut kiro_payload, tool_name_map) =
        anthropic_to_kiro(&anthropic_body, &provider, global_routes)?;

    let kiro_model = kiro_payload
        .pointer("/conversationState/currentMessage/userInputMessage/modelId")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    info!(
        request_id = request_id.as_str(),
        kiro_model,
        stream = is_stream,
        "Kiro chat/completions 请求转换完成"
    );

    // Step 3: Acquire auth
    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;
    let auth_info = acquire_kiro_auth(&state, kiro_config, tenant_refresh_token.as_deref()).await?;

    // Inject profileArn into payload if available
    if let Some(ref arn) = auth_info.profile_arn {
        if let Some(obj) = kiro_payload.as_object_mut() {
            obj.insert("profileArn".to_string(), json!(arn));
        }
    }

    // Step 4: Serialize and dispatch
    let payload_bytes = serde_json::to_vec(&kiro_payload)?;

    let dispatch_result = match dispatch_kiro_request(
        &state,
        &kiro_payload,
        &payload_bytes,
        &auth_info,
        kiro_config,
        &request_id,
        is_stream,
    )
    .await
    {
        Ok(r) => r,
        Err(AppError::UpstreamStatus(status, body)) => {
            return Ok((
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({
                    "error": {"message": body, "type": "upstream_error"}
                })),
            )
                .into_response());
        }
        Err(e) => return Err(e),
    };

    // Step 5: Convert response
    if is_stream {
        handle_stream_openai_output(
            dispatch_result.response,
            &kiro_model,
            tool_name_map,
            request_id.clone(),
            request_start,
            Instant::now(),
            dispatch_result.upstream_headers_ms,
            None,
            kiro_config.thinking_mode.as_deref(),
            std::time::Duration::from_secs(kiro_config.first_token_timeout.unwrap_or(15)),
            std::time::Duration::from_secs(kiro_config.streaming_read_timeout.unwrap_or(300)),
        )
        .await
    } else {
        // Non-streaming: collect events and build OpenAI response
        handle_kiro_non_stream_openai(
            dispatch_result.response,
            &kiro_model,
            &request_id,
            request_start,
            Instant::now(),
            dispatch_result.upstream_headers_ms,
        )
        .await
    }
}

/// Handle non-streaming Kiro response → OpenAI Chat Completions response.
async fn handle_kiro_non_stream_openai(
    upstream_resp: reqwest::Response,
    model: &str,
    request_id: &str,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
) -> Result<Response> {
    use crate::convert::kiro::eventstream::{Event, EventStreamDecoder};

    let body_bytes = upstream_resp.bytes().await?;
    let mut decoder = EventStreamDecoder::new();
    decoder
        .feed(&body_bytes)
        .map_err(|e| AppError::Request(format!("EventStream 解析错误: {}", e)))?;

    let mut text_parts = Vec::new();
    let mut thinking_parts = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut input_tokens = 0u64;
    let mut tool_call_index = 0usize;

    loop {
        match decoder.decode() {
            Ok(Some(frame)) => {
                if let Ok(event) = Event::from_frame(&frame) {
                    match event {
                        Event::AssistantResponse { content } => text_parts.push(content),
                        Event::ReasoningContent { text } => thinking_parts.push(text),
                        Event::ToolUse {
                            name,
                            tool_use_id,
                            input,
                            stop,
                        } => {
                            if stop {
                                let input_val: Value =
                                    serde_json::from_str(&input).unwrap_or(json!({}));
                                tool_calls.push(json!({
                                    "id": format!("call_{}", &tool_use_id[tool_use_id.len().min(6)..]),
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(&input_val).unwrap_or_default()
                                    }
                                }));
                                tool_call_index += 1;
                            }
                        }
                        Event::ContextUsage { percentage } => {
                            let window =
                                crate::convert::kiro::model_map::context_window_size(model);
                            input_tokens = (percentage * window as f64 / 100.0) as u64;
                        }
                        _ => {}
                    }
                }
            }
            Ok(None) => break,
            Err(_) => {
                if decoder.is_stopped() {
                    break;
                }
            }
        }
    }

    // Build OpenAI response
    let content_text = text_parts.join("");
    let thinking_text = thinking_parts.join("");
    let content_len = content_text.len();

    let mut message = json!({
        "role": "assistant",
        "content": if content_text.is_empty() { Value::Null } else { Value::String(content_text) }
    });

    if !thinking_text.is_empty() {
        message["reasoning_content"] = json!(thinking_text);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    let has_tool_use = !tool_calls.is_empty();
    let output_tokens = content_len as u64 / 4 + 1;
    let finish_reason = if has_tool_use { "tool_calls" } else { "stop" };

    let response = json!({
        "id": format!("chatcmpl-{}", next_request_id()),
        "object": "chat.completion",
        "created": now_epoch_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens
        }
    });

    info!(
        request_id,
        has_tool_use,
        input_tokens,
        output_tokens,
        upstream_headers_ms,
        upstream_total_ms = elapsed_ms(upstream_start),
        request_total_ms = elapsed_ms(request_start),
        "Kiro 非流式 OpenAI 响应完成"
    );

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))?)
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Handle `GET /v1/models` — returns available models in OpenAI format.
pub async fn proxy_models(
    State(state): State<super::state::AppState>,
) -> Result<Response> {
    use crate::convert::kiro::model_map::KIRO_MODELS;

    const MODEL_CACHE_TTL_SECS: u64 = 3600; // 1 hour

    let provider = state.current_provider();

    // Check cache first
    if let Some(ref cache_arc) = state.model_cache {
        let cache = cache_arc.lock().await;
        if cache.0.elapsed().as_secs() < MODEL_CACHE_TTL_SECS {
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(cache.1.to_string()))
                .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?);
        }
    }

    // Build model list
    let models: Vec<Value> = if provider.format == crate::config::ProviderFormat::Kiro {
        // Try to fetch from Kiro API
        let region = provider
            .kiro_config
            .as_ref()
            .and_then(|k| k.api_region.as_deref().or(Some(k.region.as_str())))
            .unwrap_or("us-east-1");
        let list_url = format!("https://runtime.{}.kiro.dev/ListAvailableModels", region);

        // Get auth token
        let token = if let Some(ref auth_arc) = state.kiro_auth {
            let mut auth = auth_arc.lock().await;
            auth.get_valid_token().await.unwrap_or_default()
        } else {
            String::new()
        };

        // Try API call
        let api_models = if !token.is_empty() {
            match state
                .client
                .get(&list_url)
                .header("Authorization", format!("Bearer {}", token))
                .header("x-amz-target", "AmazonCodeWhispererStreamingService.ListAvailableModels")
                .header("Content-Type", "application/x-amz-json-1.0")
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    resp.json::<Value>().await.ok()
                }
                _ => None,
            }
        } else {
            None
        };

        // Parse API response or fall back to static list
        if let Some(api_resp) = api_models {
            // Kiro API returns array of model objects with modelId field
            if let Some(arr) = api_resp.as_array() {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("modelId").or_else(|| m.get("id"))?.as_str()?;
                        Some(json!({
                            "id": id,
                            "object": "model",
                            "created": now_epoch_secs(),
                            "owned_by": "kiro"
                        }))
                    })
                    .collect()
            } else {
                static_kiro_model_list()
            }
        } else {
            static_kiro_model_list()
        }
    } else {
        // Non-Kiro provider: return configured model + model routes
        let mut models = vec![json!({
            "id": provider.model,
            "object": "model",
            "created": now_epoch_secs(),
            "owned_by": provider.name
        })];
        for route in provider.model_routes.iter() {
            models.push(json!({
                "id": route.target,
                "object": "model",
                "created": now_epoch_secs(),
                "owned_by": provider.name
            }));
        }
        models
    };

    let response = json!({
        "object": "list",
        "data": models
    });

    // Update cache
    if let Some(ref cache_arc) = state.model_cache {
        let mut cache = cache_arc.lock().await;
        *cache = (Instant::now(), response.clone());
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

/// Static fallback model list from KIRO_MODELS constant.
fn static_kiro_model_list() -> Vec<Value> {
    use crate::convert::kiro::model_map::KIRO_MODELS;
    KIRO_MODELS
        .iter()
        .map(|(name, _)| {
            json!({
                "id": name,
                "object": "model",
                "created": 0u64,
                "owned_by": "kiro"
            })
        })
        .collect()
}

/// Handle `POST /v1/messages/count_tokens` — estimates token count for an Anthropic request.
/// Used by Claude Code to decide conversation compaction timing.
pub async fn proxy_count_tokens(
    State(state): State<super::state::AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response> {
    // Validate auth
    check_auth(&headers, &state.config)?;

    let provider = state.current_provider();

    // Estimate tokens from the request body
    let mut total_tokens: u64 = 0;

    // System prompt tokens
    if let Some(system) = body.get("system") {
        let text = match system {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        total_tokens += crate::convert::kiro::stream::estimate_tokens(&text) as u64;
    }

    // Message tokens
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            // Role overhead
            total_tokens += 4;
            if let Some(content) = msg.get("content") {
                match content {
                    Value::String(s) => {
                        total_tokens += crate::convert::kiro::stream::estimate_tokens(s) as u64;
                    }
                    Value::Array(arr) => {
                        for block in arr {
                            match block.get("type").and_then(|v| v.as_str()) {
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                        total_tokens +=
                                            crate::convert::kiro::stream::estimate_tokens(t) as u64;
                                    }
                                }
                                Some("image") => total_tokens += 100,
                                Some("tool_use") => {
                                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                    let input = block.get("input").map(|v| v.to_string()).unwrap_or_default();
                                    total_tokens += crate::convert::kiro::stream::estimate_tokens(&format!("{} {}", name, input)) as u64;
                                }
                                Some("tool_result") => {
                                    let result_text = match block.get("content") {
                                        Some(Value::String(s)) => s.clone(),
                                        Some(Value::Array(arr)) => arr
                                            .iter()
                                            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                                            .collect::<Vec<_>>()
                                            .join("\n"),
                                        _ => String::new(),
                                    };
                                    total_tokens += crate::convert::kiro::stream::estimate_tokens(&result_text) as u64;
                                }
                                Some("thinking") => {
                                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                                        total_tokens += crate::convert::kiro::stream::estimate_tokens(t) as u64;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Tool definition tokens
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        for tool in tools {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
            total_tokens += crate::convert::kiro::stream::estimate_tokens(&format!("{} {}", name, desc)) as u64;
            // Schema tokens
            if let Some(schema) = tool.get("input_schema") {
                total_tokens += crate::convert::kiro::stream::estimate_tokens(&schema.to_string()) as u64;
            }
        }
    }

    let response = json!({
        "input_tokens": total_tokens
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

/// Handle `POST /v1/responses` — OpenAI Responses API (used by Codex CLI).
/// Converts Responses API format to Kiro format via Anthropic intermediate.
pub async fn proxy_responses(
    State(state): State<super::state::AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response> {
    use crate::convert::kiro::responses::responses_to_kiro;
    use crate::convert::kiro::stream::handle_stream_anthropic_output as kiro_handle_stream;
    use std::collections::HashMap;

    check_auth(&headers, &state.config)?;

    let tenant_refresh_token = match check_auth_tenant(&headers, &state.config) {
        AuthResult::TenantOk { refresh_token } => Some(refresh_token),
        _ => None,
    };

    let provider = state.current_provider();
    let model_routes = state.current_model_routes();
    let global_routes = model_routes.as_slice();

    if provider.format != crate::config::ProviderFormat::Kiro {
        return Err(AppError::Request(
            "/v1/responses 仅支持 Kiro provider".to_string(),
        ));
    }

    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    let (mut kiro_payload, _tool_name_map) = responses_to_kiro(&body, &provider, global_routes)?;
    let auth_info = acquire_kiro_auth(&state, kiro_config, tenant_refresh_token.as_deref()).await?;

    // Inject profileArn into payload if available
    if let Some(ref arn) = auth_info.profile_arn {
        if let Some(obj) = kiro_payload.as_object_mut() {
            obj.insert("profileArn".to_string(), json!(arn));
        }
    }

    let payload_bytes = serde_json::to_vec(&kiro_payload)?;
    let is_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let dispatch_result = match dispatch_kiro_request(
        &state,
        &kiro_payload,
        &payload_bytes,
        &auth_info,
        kiro_config,
        "responses",
        is_stream,
    )
    .await
    {
        Ok(r) => r,
        Err(AppError::UpstreamStatus(status, body)) => {
            return Ok((
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({"error": {"message": body, "type": "upstream_error"}})),
            ).into_response());
        }
        Err(e) => return Err(e),
    };

    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("kiro");

    if is_stream {
        kiro_handle_stream(
            dispatch_result.response,
            model,
            HashMap::new(),
            "responses".to_string(),
            Instant::now(),
            Instant::now(),
            dispatch_result.upstream_headers_ms,
            None,
            kiro_config.thinking_mode.as_deref(),
            std::time::Duration::from_secs(kiro_config.first_token_timeout.unwrap_or(15)),
            std::time::Duration::from_secs(kiro_config.streaming_read_timeout.unwrap_or(300)),
        )
        .await
    } else {
        let tool_map = HashMap::new();
        handle_kiro_non_stream(
            dispatch_result.response,
            model,
            &tool_map,
            "responses",
            Instant::now(),
            Instant::now(),
            dispatch_result.upstream_headers_ms,
        )
        .await
    }
}

/// Handle `GET /api/usage` — query Kiro usage/balance information.
pub async fn proxy_usage(
    State(state): State<super::state::AppState>,
) -> Result<Response> {
    let provider = state.current_provider();
    if provider.format != crate::config::ProviderFormat::Kiro {
        return Err(AppError::Request("Usage 查询仅支持 Kiro provider".to_string()));
    }

    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    // Get auth token
    let token = if let Some(ref auth_arc) = state.kiro_auth {
        let mut auth = auth_arc.lock().await;
        auth.get_valid_token().await.unwrap_or_default()
    } else {
        return Err(AppError::Request("Kiro auth 未初始化".to_string()));
    };

    let region = kiro_config.api_region.as_deref().unwrap_or(&kiro_config.region);
    let url = format!("https://q.{}.amazonaws.com/getUsageLimits", region);

    let resp = state.client
        .post(&url)
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("Authorization", format!("Bearer {}", token))
        .header("x-amz-target", "AmazonCodeWhispererStreamingService.GetUsageLimits")
        .timeout(Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body = r.text().await.unwrap_or_default();
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
        }
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            Ok((
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({"error": {"message": body, "type": "upstream_error"}})),
            ).into_response())
        }
        Err(e) => Err(AppError::Http(e)),
    }
}

/// Handle `GET /api/flows` — query flow monitor data.
pub async fn proxy_flows(
    State(state): State<super::state::AppState>,
) -> Result<Response> {
    if let Some(ref monitor) = state.flow_monitor {
        let monitor = monitor.lock().await;
        let stats = monitor.get_stats();
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&stats).unwrap_or_default()))
            .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
    } else {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":"flow monitor not enabled"}"#.to_string()))
            .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
    }
}

/// Handle `GET /api/status` — service status.
pub async fn proxy_status(
    State(state): State<super::state::AppState>,
) -> Result<Response> {
    let provider = state.current_provider();
    let kiro_auth_ok = state.kiro_auth.is_some();

    let status = json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "provider": provider.name,
        "format": format!("{:?}", provider.format),
        "kiro_auth": kiro_auth_ok,
        "account_manager": state.kiro_account_manager.is_some(),
        "flow_monitor": state.flow_monitor.is_some(),
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(status.to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

/// Handle `POST /api/kiro/login/start` — start OIDC Device Authorization Flow.
pub async fn proxy_kiro_login_start(
    State(state): State<super::state::AppState>,
) -> Result<Response> {
    use crate::convert::kiro::auth_flow::start_device_flow;

    let provider = state.current_provider();
    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    let region = &kiro_config.region;
    let result = start_device_flow(&state.client, region).await?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&json!({
            "user_code": result.user_code,
            "verification_uri": result.verification_uri,
            "verification_uri_complete": result.verification_uri_complete,
            "expires_in": result.expires_in,
            "interval": result.interval.unwrap_or(5),
        })).unwrap_or_default()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

/// Handle `POST /api/kiro/login/poll` — poll OIDC device flow for token.
pub async fn proxy_kiro_login_poll(
    State(state): State<super::state::AppState>,
    Json(body): Json<Value>,
) -> Result<Response> {
    use crate::convert::kiro::auth_flow::poll_device_token;

    let provider = state.current_provider();
    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    let client_id = body.get("client_id").and_then(|v| v.as_str()).ok_or_else(|| {
        AppError::Request("缺少 client_id".to_string())
    })?;
    let device_code = body.get("device_code").and_then(|v| v.as_str()).ok_or_else(|| {
        AppError::Request("缺少 device_code".to_string())
    })?;

    let status = poll_device_token(&state.client, &kiro_config.region, client_id, device_code).await;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&status).unwrap_or_default()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

/// Handle `POST /api/kiro/social/start` — start Social OAuth flow.
pub async fn proxy_kiro_social_start(
    Json(body): Json<Value>,
) -> Result<Response> {
    use crate::convert::kiro::auth_flow::{start_social_auth, OAuthProvider};

    let provider_str = body.get("provider").and_then(|v| v.as_str()).unwrap_or("google");
    let redirect_uri = body.get("redirect_uri").and_then(|v| v.as_str())
        .unwrap_or("http://localhost:19823/callback");
    let state_param = body.get("state").and_then(|v| v.as_str()).unwrap_or("kiro_login");

    let oauth_provider = match provider_str {
        "google" => OAuthProvider::Google,
        "github" => OAuthProvider::GitHub,
        _ => return Err(AppError::Request(format!("不支持的 OAuth provider: {}", provider_str))),
    };

    match start_social_auth(oauth_provider, redirect_uri, state_param) {
        Ok(url) => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"auth_url": url}).to_string()))
            .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?),
        Err(e) => Err(e),
    }
}

/// Handle `POST /api/kiro/social/exchange` — exchange OAuth code for Kiro token.
pub async fn proxy_kiro_social_exchange(
    State(state): State<super::state::AppState>,
    Json(body): Json<Value>,
) -> Result<Response> {
    use crate::convert::kiro::auth_flow::{exchange_social_code, exchange_for_kiro_token, OAuthProvider};

    let provider_str = body.get("provider").and_then(|v| v.as_str()).unwrap_or("google");
    let code = body.get("code").and_then(|v| v.as_str()).ok_or_else(|| {
        AppError::Request("缺少 code".to_string())
    })?;

    let oauth_provider = match provider_str {
        "google" => OAuthProvider::Google,
        "github" => OAuthProvider::GitHub,
        _ => return Err(AppError::Request(format!("不支持的 OAuth provider: {}", provider_str))),
    };

    let redirect_uri = body.get("redirect_uri").and_then(|v| v.as_str())
        .unwrap_or("http://localhost:19823/callback");

    // Exchange OAuth code for social token
    let social_token = exchange_social_code(&state.client, oauth_provider, code, redirect_uri).await?;
    let social_access = social_token.access_token.ok_or_else(|| {
        AppError::Request("OAuth token 交换失败: 无 access_token".to_string())
    })?;

    // Exchange social token for Kiro token
    let provider = state.current_provider();
    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    let (access_token, refresh_token, expires_in) =
        exchange_for_kiro_token(&state.client, &social_access, oauth_provider, &kiro_config.region).await?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({
            "access_token": access_token,
            "refresh_token": refresh_token,
            "expires_in": expires_in,
        }).to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}
