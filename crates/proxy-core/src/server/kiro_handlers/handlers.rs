//! Kiro message-processing handlers (streaming + non-streaming, Anthropic + OpenAI output).

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;
use tracing::info;

use super::dispatch::{acquire_kiro_auth, dispatch_kiro_request};
use crate::convert::anthropic_openai::stream::StreamLogContext;
use crate::server::state::{elapsed_ms, next_request_id, RequestCompletionGuard};
use crate::error::{AppError, Result};

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
