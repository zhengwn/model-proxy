//! Anthropic Messages SSE → OpenAI Chat Completions SSE streaming conversion.

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{error, info};

use super::common::{compact_sse_buffer, next_sse_block, now_epoch_secs, StreamEvents};
use super::log_context::StreamLogContext;
use super::super::request::anthropic_id_to_openai;
use crate::convert::utils::append_utf8_safe;
use crate::error::{AppError, Result};
use crate::server::state::elapsed_ms;

// ---- Anthropic SSE → OpenAI SSE conversion ----

/// State for converting Anthropic streaming responses to OpenAI SSE format.
pub(crate) struct OpenAiStreamOutputState {
    pub(crate) stream_id: String,
    pub(crate) started: bool,
    pub(crate) current_block_type: Option<String>,
    pub(crate) tool_call_counter: usize,
    /// Maps Anthropic content_block index → OpenAI tool_call index,
    /// so `input_json_delta` events route to the correct tool.
    pub(crate) block_to_tool_index: std::collections::HashMap<usize, usize>,
    pub(crate) ended: bool,
    pub(crate) output_tokens: usize,
    pub(crate) input_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) finish_reason: Option<String>,
}

impl OpenAiStreamOutputState {
    pub(crate) fn new() -> Self {
        Self {
            stream_id: String::new(),
            started: false,
            current_block_type: None,
            tool_call_counter: 0,
            block_to_tool_index: std::collections::HashMap::new(),
            ended: false,
            output_tokens: 0,
            input_tokens: 0,
            completion_tokens: 0,
            finish_reason: None,
        }
    }
}

/// Format an OpenAI SSE data-only chunk (no `event:` line).
fn openai_sse_chunk(data: &Value) -> String {
    let json_str = serde_json::to_string(data).unwrap_or_default();
    let mut s = String::with_capacity(6 + json_str.len() + 2);
    s.push_str("data: ");
    s.push_str(&json_str);
    s.push('\n');
    s.push('\n');
    s
}

fn openai_sse_chunk_literal(json_literal: &str) -> String {
    let mut s = String::with_capacity(6 + json_literal.len() + 2);
    s.push_str("data: ");
    s.push_str(json_literal);
    s.push('\n');
    s.push('\n');
    s
}

/// Convert a single Anthropic SSE event into zero or more OpenAI SSE chunks.
pub(crate) fn convert_anthropic_stream_chunk(
    event_type: &str,
    data: &Value,
    model: &str,
    state: &mut OpenAiStreamOutputState,
) -> StreamEvents {
    let mut events = StreamEvents::new();

    match event_type {
        "message_start" => {
            if let Some(message) = data.get("message") {
                let id = message
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("msg_unknown");
                state.stream_id = format!("chatcmpl-{}", id);
                state.started = true;

                // Emit initial chunk with role
                events.push(openai_sse_chunk(&json!({
                    "id": state.stream_id,
                    "object": "chat.completion.chunk",
                    "created": now_epoch_secs(),
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant"},
                        "finish_reason": null
                    }]
                })));

                // Capture usage from message_start if present
                if let Some(usage) = message.get("usage") {
                    state.input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
        }
        "content_block_start" => {
            let block = data.get("content_block");
            let block_type = block
                .and_then(|b| b.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match block_type {
                "text" => {
                    state.current_block_type = Some("text".to_string());
                }
                "thinking" => {
                    state.current_block_type = Some("thinking".to_string());
                }
                "tool_use" => {
                    state.current_block_type = Some("tool_use".to_string());
                    let tool_id = block
                        .and_then(|b| b.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let tool_name = block
                        .and_then(|b| b.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let openai_id = anthropic_id_to_openai(tool_id);
                    let openai_index = state.tool_call_counter;
                    state.tool_call_counter += 1;

                    // Record the mapping from Anthropic block index to OpenAI tool index
                    let anthropic_block_index = data.get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(openai_index as u64) as usize;
                    state.block_to_tool_index.insert(anthropic_block_index, openai_index);

                    events.push(openai_sse_chunk(&json!({
                        "id": state.stream_id,
                        "object": "chat.completion.chunk",
                        "created": now_epoch_secs(),
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [{
                                    "index": openai_index,
                                    "id": openai_id,
                                    "type": "function",
                                    "function": {
                                        "name": tool_name,
                                        "arguments": ""
                                    }
                                }]
                            },
                            "finish_reason": null
                        }]
                    })));
                }
                _ => {}
            }
        }
        "content_block_delta" => {
            let delta = data.get("delta");
            let delta_type = delta
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match delta_type {
                "text_delta" => {
                    if let Some(text) = delta
                        .and_then(|d| d.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        if !text.is_empty() {
                            events.push(openai_sse_chunk(&json!({
                                "id": state.stream_id,
                                "object": "chat.completion.chunk",
                                "created": now_epoch_secs(),
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": {"content": text},
                                    "finish_reason": null
                                }]
                            })));
                            state.output_tokens += text.len() / 4 + 1;
                        }
                    }
                }
                "thinking_delta" => {
                    if let Some(thinking) = delta
                        .and_then(|d| d.get("thinking"))
                        .and_then(|v| v.as_str())
                    {
                        if !thinking.is_empty() {
                            events.push(openai_sse_chunk(&json!({
                                "id": state.stream_id,
                                "object": "chat.completion.chunk",
                                "created": now_epoch_secs(),
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": {"reasoning_content": thinking},
                                    "finish_reason": null
                                }]
                            })));
                            state.output_tokens += thinking.len() / 4 + 1;
                        }
                    }
                }
                "input_json_delta" => {
                    if let Some(partial_json) = delta
                        .and_then(|d| d.get("partial_json"))
                        .and_then(|v| v.as_str())
                    {
                        if !partial_json.is_empty() {
                            // Look up the correct OpenAI tool_call index from the Anthropic block index
                            let anthropic_block_index = data.get("index")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as usize;
                            let index = state.block_to_tool_index
                                .get(&anthropic_block_index)
                                .copied()
                                .unwrap_or_else(|| state.tool_call_counter.saturating_sub(1));
                            events.push(openai_sse_chunk(&json!({
                                "id": state.stream_id,
                                "object": "chat.completion.chunk",
                                "created": now_epoch_secs(),
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": {
                                        "tool_calls": [{
                                            "index": index,
                                            "function": {
                                                "arguments": partial_json
                                            }
                                        }]
                                    },
                                    "finish_reason": null
                                }]
                            })));
                        }
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            state.current_block_type = None;
        }
        "message_delta" => {
            // Extract stop_reason and usage from message_delta
            let delta = data.get("delta");
            if let Some(sr) = delta
                .and_then(|d| d.get("stop_reason"))
                .and_then(|v| v.as_str())
            {
                state.finish_reason = Some(match sr {
                    "end_turn" | "pause_turn" => "stop".to_string(),
                    "max_tokens" => "length".to_string(),
                    "tool_use" => "tool_calls".to_string(),
                    "refusal" => "content_filter".to_string(),
                    _ => "stop".to_string(),
                });
            }

            if let Some(usage) = data.get("usage") {
                if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                    state.completion_tokens = ot;
                }
            }

            // Build the final chunk with finish_reason and usage
            let finish = state.finish_reason.as_deref().unwrap_or("stop");
            let mut chunk = json!({
                "id": state.stream_id,
                "object": "chat.completion.chunk",
                "created": now_epoch_secs(),
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish
                }],
                "usage": {
                    "prompt_tokens": state.input_tokens,
                    "completion_tokens": state.completion_tokens,
                    "total_tokens": state.input_tokens + state.completion_tokens
                }
            });

            // Include cache info if present
            if let Some(usage) = data.get("usage") {
                if let Some(cache_read) = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
                    chunk["usage"]["prompt_cache_hit_tokens"] = json!(cache_read);
                    let details = chunk["usage"]
                        .as_object_mut()
                        .unwrap()
                        .entry("prompt_tokens_details".to_string())
                        .or_insert_with(|| json!({}));
                    details["cached_tokens"] = json!(cache_read);
                }
                if let Some(cache_creation) = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) {
                    let details = chunk["usage"]
                        .as_object_mut()
                        .unwrap()
                        .entry("prompt_tokens_details".to_string())
                        .or_insert_with(|| json!({}));
                    details["cache_creation_input_tokens"] = json!(cache_creation);
                }
            }

            events.push(openai_sse_chunk(&chunk));
            state.ended = true;
        }
        "message_stop" => {
            // No direct OpenAI equivalent; the [DONE] marker is sent by the handler
        }
        "ping" => {
            // Ignore ping events
        }
        _ => {
            // Ignore unknown event types
        }
    }

    events
}

/// Parse an Anthropic SSE block to extract event type and JSON data.
fn parse_anthropic_sse_block(block: &str) -> Option<(String, Value)> {
    let mut event_type = String::new();
    let mut data_str = String::new();

    for line in block.lines() {
        if let Some(et) = line.strip_prefix("event: ") {
            event_type = et.trim().to_string();
        } else if let Some(d) = line.strip_prefix("data: ") {
            if !data_str.is_empty() {
                data_str.push('\n');
            }
            data_str.push_str(d.trim());
        }
    }

    if data_str.is_empty() {
        return None;
    }

    let data: Value = serde_json::from_str(&data_str).ok()?;
    Some((event_type, data))
}

/// Handle an Anthropic upstream streaming response, converting it to OpenAI SSE format.
pub(crate) async fn handle_stream_openai_output(
    upstream_resp: reqwest::Response,
    model: &str,
    request_id: String,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
    log_ctx: Option<StreamLogContext>,
) -> Result<Response> {
    info!(
        request_id = request_id.as_str(),
        upstream_headers_ms, "开始处理上游 Anthropic 流式响应，转换为 OpenAI 格式"
    );
    let byte_stream = upstream_resp.bytes_stream();
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, AppError>>(128);
    let model = model.to_string();

    tokio::spawn(async move {
        let stream_start = Instant::now();
        let mut buffer = String::with_capacity(16384);
        let mut utf8_remainder: Vec<u8> = Vec::new();
        let mut state = OpenAiStreamOutputState::new();
        let mut read_offset: usize = 0;
        let mut upstream_chunks: u64 = 0;
        let mut emitted_events: u64 = 0;

        macro_rules! log_openai_stream_end {
            ($reason:expr) => {{
                let (status, error_message) = match $reason {
                    "done" | "eof_after_finish" | "eof_without_done" => (200u16, None),
                    "client_disconnected" => {
                        (499, Some("stream ended: client disconnected".to_string()))
                    }
                    "upstream_error" => (502, Some("stream ended: upstream error".to_string())),
                    other => (502, Some(format!("stream ended: {}", other))),
                };
                info!(
                    request_id = request_id.as_str(),
                    stream_id = state.stream_id.as_str(),
                    reason = $reason,
                    started = state.started,
                    ended = state.ended,
                    upstream_chunks = upstream_chunks,
                    emitted_events = emitted_events,
                    input_tokens = state.input_tokens,
                    output_tokens = state.completion_tokens,
                    stop_reason = state.finish_reason.as_deref().unwrap_or(""),
                    upstream_headers_ms,
                    upstream_total_ms = elapsed_ms(upstream_start),
                    stream_total_ms = elapsed_ms(stream_start),
                    request_total_ms = elapsed_ms(request_start),
                    "Anthropic→OpenAI 流式响应结束"
                );
                if let Some(ref log_ctx) = log_ctx {
                    log_ctx.emit(
                        status,
                        Some(upstream_headers_ms as u64),
                        error_message,
                        None,
                        Some(state.input_tokens + state.completion_tokens),
                    );
                }
            }};
        }

        let mut stream = byte_stream;

        while let Some(result) = stream.next().await {
            match result {
                Ok(bytes) => {
                    append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);

                    while let Some(block) = next_sse_block(&buffer, &mut read_offset) {
                        let Some((event_type, data)) = parse_anthropic_sse_block(block) else {
                            continue;
                        };

                        upstream_chunks += 1;

                        let events = convert_anthropic_stream_chunk(
                            &event_type,
                            &data,
                            &model,
                            &mut state,
                        );

                        for event in events {
                            match tx.send(Ok(Bytes::from(event))).await {
                                Ok(_) => emitted_events += 1,
                                Err(_) => {
                                    log_openai_stream_end!("client_disconnected");
                                    return;
                                }
                            }
                        }
                    }

                    compact_sse_buffer(&mut buffer, &mut read_offset);
                }
                Err(e) => {
                    error!(
                        request_id = request_id.as_str(),
                        error = %e,
                        upstream_headers_ms,
                        upstream_total_ms = elapsed_ms(upstream_start),
                        stream_total_ms = elapsed_ms(stream_start),
                        request_total_ms = elapsed_ms(request_start),
                        "上游 Anthropic 流式读取错误"
                    );
                    let _ = tx.send(Err(AppError::Http(e))).await;
                    log_openai_stream_end!("upstream_error");
                    return;
                }
            }
        }

        // Stream ended — send final events
        if state.started && !state.ended {
            let finish = state.finish_reason.as_deref().unwrap_or("stop");
            let chunk = openai_sse_chunk(&json!({
                "id": state.stream_id,
                "object": "chat.completion.chunk",
                "created": now_epoch_secs(),
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish
                }],
                "usage": {
                    "prompt_tokens": state.input_tokens,
                    "completion_tokens": state.completion_tokens,
                    "total_tokens": state.input_tokens + state.completion_tokens
                }
            }));
            let _ = tx.send(Ok(Bytes::from(chunk))).await;
            emitted_events += 1;
        }

        // Send [DONE]
        let done = openai_sse_chunk_literal("[DONE]");
        if tx.send(Ok(Bytes::from(done))).await.is_ok() {
            emitted_events += 1;
        }

        let reason = if state.started && state.ended {
            "done"
        } else if state.started {
            "eof_without_done"
        } else {
            "eof_before_start"
        };
        log_openai_stream_end!(reason);
    });

    let body_stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(Ok(bytes)) => Some((Ok::<Bytes, AppError>(bytes), rx)),
            Some(Err(e)) => Some((Err::<Bytes, _>(e), rx)),
            None => None,
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body_stream))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))
}
