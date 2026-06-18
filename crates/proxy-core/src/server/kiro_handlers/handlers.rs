//! Kiro message-processing handlers (streaming + non-streaming, Anthropic + OpenAI output).

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;
use tracing::{debug, info, warn};

use super::dispatch::{acquire_kiro_auth, dispatch_kiro_request, KiroDispatchResult};
use crate::convert::anthropic_openai::stream::StreamLogContext;
use crate::server::state::{elapsed_ms, next_request_id, RequestCompletionGuard};
use crate::error::{AppError, Result};

/// Result of a successful first-token retry dispatch.
struct FirstChunkDispatchResult {
    upstream_headers_ms: u128,
    initial_bytes: Bytes,
    remaining_bytes_stream: std::pin::Pin<Box<dyn futures::Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>,
}

/// Dispatch a Kiro request with first-token timeout retry.
///
/// After getting the HTTP response, reads the first byte chunk with a timeout.
/// If the first chunk arrives in time, returns it along with the remaining stream.
/// If the timeout fires, drops the response and retries the dispatch (up to `max_retries`).
async fn dispatch_with_first_token_retry(
    state: &crate::server::state::AppState,
    kiro_payload: &Value,
    payload_bytes: &[u8],
    auth_info: &super::dispatch::KiroAuthInfo,
    kiro_config: &crate::config::KiroConfig,
    request_id: &str,
    first_token_timeout: std::time::Duration,
    max_retries: u32,
) -> crate::error::Result<FirstChunkDispatchResult> {
    for attempt in 0..=max_retries {
        let dispatch_result = dispatch_kiro_request(
            state,
            kiro_payload,
            payload_bytes,
            auth_info,
            kiro_config,
            request_id,
            true,
        )
        .await?;

        let upstream_headers_ms = dispatch_result.upstream_headers_ms;
        let mut byte_stream = dispatch_result.response.bytes_stream();

        match tokio::time::timeout(first_token_timeout, byte_stream.next()).await {
            Ok(Some(Ok(first_bytes))) => {
                if attempt > 0 {
                    info!(request_id, attempt, "首 Token 重试成功");
                }
                let remaining = Box::pin(byte_stream);
                return Ok(FirstChunkDispatchResult {
                    upstream_headers_ms,
                    initial_bytes: first_bytes,
                    remaining_bytes_stream: remaining,
                });
            }
            Ok(Some(Err(e))) => return Err(crate::error::AppError::Http(e)),
            Ok(None) => {
                return Ok(FirstChunkDispatchResult {
                    upstream_headers_ms,
                    initial_bytes: Bytes::new(),
                    remaining_bytes_stream: Box::pin(futures::stream::empty()),
                });
            }
            Err(_timeout) => {
                warn!(
                    request_id,
                    attempt,
                    timeout_secs = first_token_timeout.as_secs(),
                    "首 Token 超时，重试中"
                );
                continue;
            }
        }
    }

    // All retries exhausted — dispatch once more without timeout check
    warn!(request_id, max_retries, "首 Token 重试耗尽，执行最后一次请求（无超时检查）");
    let dispatch_result = dispatch_kiro_request(
        state, kiro_payload, payload_bytes, auth_info, kiro_config, request_id, true,
    )
    .await?;

    Ok(FirstChunkDispatchResult {
        upstream_headers_ms: dispatch_result.upstream_headers_ms,
        initial_bytes: Bytes::new(),
        remaining_bytes_stream: Box::pin(dispatch_result.response.bytes_stream()),
    })
}

/// Handle `/v1/messages` request with Kiro as upstream provider.
/// This is a separate function because Kiro uses a completely different
/// request/response protocol (AWS EventStream instead of SSE).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_kiro_messages(
    state: crate::server::state::AppState,
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
    let global_routes = if state.is_model_routes_enabled() { model_routes.as_slice() } else { &[] };

    request_guard.set_phase("kiro_convert");

    // Convert Anthropic request to Kiro payload
    let (mut kiro_payload, tool_name_map) = anthropic_to_kiro(&body_json, &provider, global_routes)?;

    // Inject truncation recovery messages if any pending
    if let Some(ref kiro) = state.kiro {
        let recovery_msgs = crate::convert::kiro::truncation::build_recovery_messages(&kiro.truncation_state).await;
        if !recovery_msgs.is_empty() {
            if let Some(history) = kiro_payload
                .pointer_mut("/conversationState/history")
                .and_then(|v| v.as_array_mut())
            {
                for msg in recovery_msgs {
                    history.push(msg);
                }
                info!(
                    request_id = request_id.as_str(),
                    count = history.len(),
                    "注入截断恢复消息到 Kiro 历史"
                );
            }
        }
    }

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

    // Debug: save request payloads to disk for protocol troubleshooting
    if kiro_config.debug_save_requests.unwrap_or(false) {
        if let Ok(debug_dir) = std::env::var("MODEL_PROXY_DEBUG_DIR") {
            let dir = std::path::PathBuf::from(debug_dir).join("debug_requests");
            let _ = std::fs::create_dir_all(&dir);
            let filename = format!("{}.json", request_id);
            let debug_data = json!({
                "request_id": request_id,
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "original_request": body_json,
                "kiro_payload": kiro_payload,
            });
            let _ = std::fs::write(
                dir.join(filename),
                serde_json::to_string_pretty(&debug_data).unwrap_or_default(),
            );
        }
    }

    // Dispatch to Kiro with endpoint fallback
    request_guard.set_phase("kiro_send");
    let first_token_timeout = std::time::Duration::from_secs(kiro_config.first_token_timeout.unwrap_or(15));
    let first_token_max_retries = kiro_config.first_token_max_retries.unwrap_or(3);

    // Handle response
    request_guard.set_phase("kiro_response");

    if is_stream {
        // Streaming: dispatch with first-token timeout retry
        let first_chunk_result = match dispatch_with_first_token_retry(
            &state,
            &kiro_payload,
            &payload_bytes,
            &auth_info,
            kiro_config,
            &request_id,
            first_token_timeout,
            first_token_max_retries,
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
                        "error": { "type": "upstream_error", "message": body }
                    })),
                ).into_response());
            }
            Err(e) => { request_guard.complete(); return Err(e); }
        };

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
        let streaming_read_timeout = std::time::Duration::from_secs(kiro_config.streaming_read_timeout.unwrap_or(300));
        let thinking_mode = kiro_config.thinking_mode.as_deref();
        let initial_bytes = if first_chunk_result.initial_bytes.is_empty() {
            None
        } else {
            Some(first_chunk_result.initial_bytes)
        };
        let upstream_headers_ms = first_chunk_result.upstream_headers_ms;

        // Build a synthetic reqwest::Response from the remaining byte stream
        let remaining_body = reqwest::Body::wrap_stream(
            first_chunk_result.remaining_bytes_stream.map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
        );
        let fake_resp = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/vnd.amazon.eventstream")
                .body(remaining_body)
                .unwrap()
        );

        let response = if buffered {
            handle_stream_anthropic_output_buffered(
                fake_resp, &kiro_model, tool_name_map, request_id.clone(),
                request_start, Instant::now(), upstream_headers_ms,
                stream_log_ctx, thinking_mode, first_token_timeout, streaming_read_timeout,
                state.kiro.as_ref().map(|k| k.truncation_state.clone()), initial_bytes,
            ).await
        } else {
            handle_stream_anthropic_output(
                fake_resp, &kiro_model, tool_name_map, request_id.clone(),
                request_start, Instant::now(), upstream_headers_ms,
                stream_log_ctx, thinking_mode, first_token_timeout, streaming_read_timeout,
                state.kiro.as_ref().map(|k| k.truncation_state.clone()), initial_bytes,
            ).await
        };
        request_guard.complete();
        response
    } else {
        // Non-streaming: dispatch with CONTENT_TOO_LONG retry support
        let mut kiro_payload = kiro_payload; // make mutable for retry
        let mut payload_bytes = payload_bytes;
        let dispatch_result = match dispatch_kiro_request(
            &state, &kiro_payload, &payload_bytes, &auth_info, kiro_config, &request_id, false,
        ).await {
            Ok(r) => r,
            Err(AppError::UpstreamStatus(status, body))
                if status == 400 && crate::convert::kiro::truncation::is_content_length_exceeded(&body) =>
            {
                // CONTENT_TOO_LONG: try Smart Summary + tiered truncation retry
                warn!(request_id = request_id, "Kiro API CONTENT_LENGTH_EXCEEDS，尝试 Smart Summary 重试");

                let mut retry_result: Option<KiroDispatchResult> = None;

                // Step 1: Try LLM Smart Summary (if enabled)
                if kiro_config.smart_summary_enabled.unwrap_or(false) {
                    if let Some(ref kiro_state) = state.kiro {
                        let region = kiro_config.api_region.as_deref().unwrap_or(&kiro_config.region);
                        let client = kiro_state.client_for_proxy(kiro_config.proxy_url.as_deref()).await.unwrap_or_else(|_| state.client.clone());
                        match crate::convert::kiro::smart_summary::summarize_and_replace_history(
                            &mut kiro_payload,
                            &auth_info.token,
                            &auth_info.amz_user_agent,
                            &auth_info.user_agent,
                            region,
                            &client,
                            &kiro_state.summary_cache,
                        ).await {
                            Ok(true) => {
                                info!(request_id = request_id, "Smart Summary 成功，重试请求");
                                payload_bytes = serde_json::to_vec(&kiro_payload)?;
                                match dispatch_kiro_request(&state, &kiro_payload, &payload_bytes, &auth_info, kiro_config, &request_id, false).await {
                                    Ok(r) => retry_result = Some(r),
                                    Err(e) => warn!(error = %e, "Smart Summary 重试仍失败"),
                                }
                            }
                            Ok(false) => debug!("History too short for summary"),
                            Err(e) => warn!(error = %e, "Smart Summary 失败，降级到 tiered truncation"),
                        }
                    }
                }

                // Step 2: Tiered truncation fallback (if summary didn't work or disabled)
                if retry_result.is_none() {
                    for tier in 0..crate::convert::kiro::truncation::TRUNCATION_TIERS.len() {
                        crate::convert::kiro::smart_summary::apply_tiered_truncation(&mut kiro_payload, tier);
                        payload_bytes = serde_json::to_vec(&kiro_payload)?;
                        match dispatch_kiro_request(&state, &kiro_payload, &payload_bytes, &auth_info, kiro_config, &request_id, false).await {
                            Ok(r) => {
                                retry_result = Some(r);
                                break;
                            }
                            Err(AppError::UpstreamStatus(_s, b)) if crate::convert::kiro::truncation::is_content_length_exceeded(&b) => {
                                warn!(request_id = request_id, tier, "Tier {} truncation 仍超限，继续", tier);
                                continue;
                            }
                            Err(e) => {
                                request_guard.complete();
                                return Err(e);
                            }
                        }
                    }
                }

                match retry_result {
                    Some(r) => r,
                    None => {
                        request_guard.complete();
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "type": "error",
                                "error": { "type": "content_too_long", "message": "Payload too large even after truncation" }
                            })),
                        ).into_response());
                    }
                }
            }
            Err(AppError::UpstreamStatus(status, body)) if status == 400 => {
                // Generic 400 (not CONTENT_LENGTH_EXCEEDS): try deep sanitize → aggressive sanitize → give up
                warn!(request_id = request_id, body_len = body.len(), "Kiro API 返回 400，尝试 sanitize 重试");

                // Step 1: Try deep_sanitize (fill empty content, dedup tool results, repair orphans)
                if let Some(history) = kiro_payload
                    .pointer_mut("/conversationState/history")
                    .and_then(|v| v.as_array_mut())
                {
                    if crate::convert::kiro::sanitize::deep_sanitize(history) {
                        payload_bytes = serde_json::to_vec(&kiro_payload)?;
                        match dispatch_kiro_request(&state, &kiro_payload, &payload_bytes, &auth_info, kiro_config, &request_id, false).await {
                            Ok(r) => {
                                info!(request_id = request_id, "deep_sanitize 重试成功");
                                let response = handle_kiro_non_stream(
                                    r.response, &kiro_model, &tool_name_map, &request_id,
                                    request_start, Instant::now(), r.upstream_headers_ms,
                                ).await;
                                request_guard.complete();
                                return response;
                            }
                            Err(AppError::UpstreamStatus(s2, _)) if s2 == 400 => {
                                debug!("deep_sanitize 重试仍返回 400，降级到 aggressive_sanitize");
                            }
                            Err(e) => { request_guard.complete(); return Err(e); }
                        }
                    }
                }

                // Step 2: Try aggressive_sanitize (strip ALL tool history)
                if let Some(history) = kiro_payload
                    .pointer_mut("/conversationState/history")
                    .and_then(|v| v.as_array_mut())
                {
                    if crate::convert::kiro::sanitize::aggressive_sanitize(history) {
                        payload_bytes = serde_json::to_vec(&kiro_payload)?;
                        match dispatch_kiro_request(&state, &kiro_payload, &payload_bytes, &auth_info, kiro_config, &request_id, false).await {
                            Ok(r) => {
                                info!(request_id = request_id, "aggressive_sanitize 重试成功");
                                let response = handle_kiro_non_stream(
                                    r.response, &kiro_model, &tool_name_map, &request_id,
                                    request_start, Instant::now(), r.upstream_headers_ms,
                                ).await;
                                request_guard.complete();
                                return response;
                            }
                            Err(e) => {
                                warn!(error = %e, "aggressive_sanitize 重试仍失败");
                                request_guard.complete();
                                return Err(e);
                            }
                        }
                    }
                }

                // Both sanitize attempts didn't help — return original error
                request_guard.complete();
                return Ok((
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(json!({
                        "type": "error", "error": { "type": "upstream_error", "message": body }
                    })),
                ).into_response());
            }
            Err(AppError::UpstreamStatus(status, body)) => {
                request_guard.complete();
                return Ok((
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(json!({
                        "type": "error", "error": { "type": "upstream_error", "message": body }
                    })),
                ).into_response());
            }
            Err(e) => { request_guard.complete(); return Err(e); }
        };
        let response = handle_kiro_non_stream(
            dispatch_result.response, &kiro_model, &tool_name_map, &request_id,
            request_start, Instant::now(), dispatch_result.upstream_headers_ms,
        ).await;
        request_guard.complete();
        response
    }
}

/// Handle non-streaming Kiro response → Anthropic Messages response.
pub(crate) async fn handle_kiro_non_stream(
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
                        }
                            if stop => {
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
    // If binary decoder produced nothing, try text fallback (proxy chain may have corrupted framing)
    if text_parts.is_empty() && thinking_parts.is_empty() && tool_uses.is_empty() && input_tokens == 0 {
        warn!(request_id = request_id, "Binary EventStream 解析无结果，尝试 JSON text 回退");
        let fallback_events = crate::convert::kiro::eventstream::try_parse_text_events(&body_bytes);
        for event in fallback_events {
            match event {
                crate::convert::kiro::eventstream::Event::AssistantResponse { content } => {
                    text_parts.push(content);
                }
                crate::convert::kiro::eventstream::Event::ToolUse { name, tool_use_id, input, stop } if stop => {
                    let original_name = tool_name_map.get(name.as_str()).map(|s| s.as_str()).unwrap_or(name.as_str());
                    let input_val: Value = serde_json::from_str(&input).unwrap_or(json!({}));
                    tool_uses.push(json!({"type": "tool_use", "id": tool_use_id, "name": original_name, "input": input_val}));
                }
                _ => {}
            }
        }
        if !text_parts.is_empty() || !tool_uses.is_empty() {
            info!(request_id = request_id, "Text fallback 成功恢复内容");
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
    let output_tokens: u64 = crate::convert::kiro::stream::estimate_tokens(&visible_text) as u64;

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

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))
}

/// Handle `/v1/chat/completions` with Kiro as upstream.
/// Flow: OpenAI request → Anthropic format → Kiro payload → Kiro API → response → OpenAI format
pub(crate) async fn handle_kiro_chat_completions(
    state: crate::server::state::AppState,
    body_json: Value,
    _requested_model: String,
    request_id: String,
    request_start: Instant,
    tenant_refresh_token: Option<String>,
) -> Result<Response> {
    use crate::convert::anthropic_openai::request::openai_to_anthropic;
    use crate::convert::kiro::request::anthropic_to_kiro;
    use crate::convert::kiro::stream_openai::handle_stream_openai_output;

    let provider = state.current_provider();
    let model_routes = state.current_model_routes();
    let global_routes = if state.is_model_routes_enabled() { model_routes.as_slice() } else { &[] };

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

    let first_token_timeout = std::time::Duration::from_secs(kiro_config.first_token_timeout.unwrap_or(15));
    let first_token_max_retries = kiro_config.first_token_max_retries.unwrap_or(3);

    // Step 5: Convert response
    if is_stream {
        let first_chunk_result = match dispatch_with_first_token_retry(
            &state, &kiro_payload, &payload_bytes, &auth_info, kiro_config,
            &request_id, first_token_timeout, first_token_max_retries,
        ).await {
            Ok(r) => r,
            Err(AppError::UpstreamStatus(status, body)) => {
                return Ok((
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(json!({"error": {"message": body, "type": "upstream_error"}})),
                ).into_response());
            }
            Err(e) => return Err(e),
        };

        let initial_bytes = if first_chunk_result.initial_bytes.is_empty() {
            None
        } else {
            Some(first_chunk_result.initial_bytes)
        };
        let remaining_body = reqwest::Body::wrap_stream(
            first_chunk_result.remaining_bytes_stream.map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
        );
        let fake_resp = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/vnd.amazon.eventstream")
                .body(remaining_body)
                .unwrap()
        );

        handle_stream_openai_output(
            fake_resp, &kiro_model, tool_name_map, request_id.clone(),
            request_start, Instant::now(), first_chunk_result.upstream_headers_ms,
            None, kiro_config.thinking_mode.as_deref(),
            first_token_timeout, std::time::Duration::from_secs(kiro_config.streaming_read_timeout.unwrap_or(300)),
            initial_bytes,
        ).await
    } else {
        let dispatch_result = match dispatch_kiro_request(
            &state, &kiro_payload, &payload_bytes, &auth_info, kiro_config, &request_id, false,
        ).await {
            Ok(r) => r,
            Err(AppError::UpstreamStatus(status, body)) => {
                return Ok((
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(json!({"error": {"message": body, "type": "upstream_error"}})),
                ).into_response());
            }
            Err(e) => return Err(e),
        };
        handle_kiro_non_stream_openai(
            dispatch_result.response, &kiro_model, &request_id,
            request_start, Instant::now(), dispatch_result.upstream_headers_ms,
        ).await
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
                        }
                            if stop => {
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

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))
}

fn now_epoch_secs() -> u64 {
    crate::server::state::now_epoch_secs()
}
