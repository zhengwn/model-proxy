//! Kiro EventStream → Anthropic/OpenAI SSE streaming conversion.
//!
//! Reads AWS EventStream binary frames from the upstream Kiro API,
//! parses events, and converts them to Anthropic Messages SSE format.

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::eventstream::{Event, EventStreamDecoder};
use super::model_map::context_window_size;
use super::thinking_parser::{ThinkingHandlingMode, ThinkingOutput, ThinkingParser};
use crate::convert::anthropic_openai::stream::StreamLogContext;
use crate::error::{AppError, Result};
use crate::server::state::elapsed_ms;

// ---- SSE helpers ----

fn sse_event(event: &str, data: &Value) -> String {
    let json_str = serde_json::to_string(data).unwrap_or_default();
    let mut s = String::with_capacity(8 + event.len() + 8 + json_str.len() + 2);
    s.push_str("event: ");
    s.push_str(event);
    s.push_str("\ndata: ");
    s.push_str(&json_str);
    s.push_str("\n\n");
    s
}

fn sse_event_literal(event: &str, json_literal: &str) -> String {
    let mut s = String::with_capacity(8 + event.len() + 8 + json_literal.len() + 2);
    s.push_str("event: ");
    s.push_str(event);
    s.push_str("\ndata: ");
    s.push_str(json_literal);
    s.push_str("\n\n");
    s
}

fn sse_content_block_stop(index: usize) -> String {
    sse_event_literal(
        "content_block_stop",
        &format!(
            "{{\"type\":\"content_block_stop\",\"index\":{}}}",
            index
        ),
    )
}

// ---- SSE keep-alive constants ----

/// Interval between keep-alive pings when the stream is idle.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(25);
/// Overall stream timeout when no real data arrives (upstream stall).
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(300);
/// Raw bytes sent as an SSE keep-alive comment (ignored by compliant parsers).
const KEEP_ALIVE_BYTES: &[u8] = b": keepalive\n\n";

// ---- Stream conversion state ----

/// State machine for converting Kiro events to Anthropic SSE.
struct AnthropicStreamState {
    stream_id: String,
    started: bool,
    ended: bool,
    current_block_type: Option<String>, // "text" | "thinking" | "tool_use"
    current_block_index: Option<usize>,
    next_block_index: usize,
    tool_block_indices: HashMap<String, usize>, // tool_use_id → block_index
    open_tool_blocks: Vec<usize>,
    has_tool_use: bool,
    stop_reason: Option<String>,
    output_tokens: usize,
    input_tokens: u64,
    completion_tokens: u64,
    last_content: String,  // for text dedup
    in_thinking: bool,
    tool_input_buffers: HashMap<String, String>, // tool_use_id → accumulated input
    thinking_parser: Option<ThinkingParser>,      // FSM for thinking tag extraction
}

impl AnthropicStreamState {
    fn new() -> Self {
        Self {
            stream_id: format!("msg_{}", generate_id()),
            started: false,
            ended: false,
            current_block_type: None,
            current_block_index: None,
            next_block_index: 0,
            tool_block_indices: HashMap::new(),
            open_tool_blocks: Vec::new(),
            has_tool_use: false,
            stop_reason: None,
            output_tokens: 0,
            input_tokens: 0,
            completion_tokens: 0,
            last_content: String::new(),
            in_thinking: false,
            tool_input_buffers: HashMap::new(),
            thinking_parser: None,
        }
    }

    fn alloc_block_index(&mut self) -> usize {
        let idx = self.next_block_index;
        self.next_block_index += 1;
        idx
    }

    fn stop_current_block(&mut self, events: &mut Vec<String>) {
        if let Some(idx) = self.current_block_index.take() {
            events.push(sse_content_block_stop(idx));
        }
        self.current_block_type = None;
    }

    fn close_open_tool_blocks(&mut self, events: &mut Vec<String>) {
        for idx in self.open_tool_blocks.drain(..) {
            events.push(sse_content_block_stop(idx));
        }
    }

    fn get_stop_reason(&self) -> &str {
        if let Some(sr) = &self.stop_reason {
            return sr.as_str();
        }
        if self.has_tool_use {
            "tool_use"
        } else {
            "end_turn"
        }
    }
}

// ---- Event → SSE conversion ----

fn process_event(
    event: &Event,
    model: &str,
    state: &mut AnthropicStreamState,
    tool_name_map: &HashMap<String, String>,
) -> Vec<String> {
    let mut events = Vec::new();

    match event {
        Event::AssistantResponse { content } => {
            if content.is_empty() {
                return events;
            }

            // Text dedup: skip if this is a prefix of previous content
            if state.last_content.len() >= content.len()
                && state.last_content[..content.len()] == *content
            {
                return events;
            }
            // Extract only the new part
            let new_text = if content.starts_with(&state.last_content) && !state.last_content.is_empty() {
                &content[state.last_content.len()..]
            } else {
                content.as_str()
            };
            state.last_content = content.clone();

            if new_text.is_empty() {
                return events;
            }

            // Emit message_start if not yet started
            if !state.started {
                state.started = true;
                events.push(sse_event(
                    "message_start",
                    &json!({
                        "type": "message_start",
                        "message": {
                            "id": state.stream_id,
                            "type": "message",
                            "role": "assistant",
                            "model": model,
                            "usage": {"input_tokens": 0, "output_tokens": 0}
                        }
                    }),
                ));
            }

            // Close thinking block if we were in one
            if state.in_thinking {
                state.in_thinking = false;
                state.stop_current_block(&mut events);
            }
            state.close_open_tool_blocks(&mut events);

            // Use ThinkingParser if configured, otherwise emit directly
            if let Some(ref mut parser) = state.thinking_parser {
                let outputs = parser.feed(new_text);
                for output in outputs {
                    match output {
                        ThinkingOutput::ThinkingDelta(thinking_text) => {
                            if !state.in_thinking {
                                state.stop_current_block(&mut events);
                                let idx = state.alloc_block_index();
                                events.push(sse_event(
                                    "content_block_start",
                                    &json!({
                                        "type": "content_block_start",
                                        "index": idx,
                                        "content_block": {"type": "thinking", "thinking": ""}
                                    }),
                                ));
                                state.current_block_type = Some("thinking".to_string());
                                state.current_block_index = Some(idx);
                                state.in_thinking = true;
                            }
                            let idx = state.current_block_index.unwrap_or(0);
                            events.push(sse_event(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta",
                                    "index": idx,
                                    "delta": {"type": "thinking_delta", "thinking": thinking_text}
                                }),
                            ));
                        }
                        ThinkingOutput::ContentDelta(text) => {
                            if state.in_thinking {
                                state.in_thinking = false;
                                state.stop_current_block(&mut events);
                            }
                            if state.current_block_type.as_deref() != Some("text") {
                                state.stop_current_block(&mut events);
                                let idx = state.alloc_block_index();
                                events.push(sse_event(
                                    "content_block_start",
                                    &json!({
                                        "type": "content_block_start",
                                        "index": idx,
                                        "content_block": {"type": "text", "text": ""}
                                    }),
                                ));
                                state.current_block_type = Some("text".to_string());
                                state.current_block_index = Some(idx);
                            }
                            let idx = state.current_block_index.unwrap_or(0);
                            events.push(sse_event(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta",
                                    "index": idx,
                                    "delta": {"type": "text_delta", "text": text}
                                }),
                            ));
                            state.output_tokens += estimate_tokens(&text);
                        }
                        ThinkingOutput::None => {}
                    }
                }
            } else {
                // Direct text emission without ThinkingParser
                if state.current_block_type.as_deref() != Some("text") {
                    state.stop_current_block(&mut events);
                    let idx = state.alloc_block_index();
                    events.push(sse_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    ));
                    state.current_block_type = Some("text".to_string());
                    state.current_block_index = Some(idx);
                }

                let idx = state.current_block_index.unwrap_or(0);
                events.push(sse_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "text_delta", "text": new_text}
                    }),
                ));
                state.output_tokens += estimate_tokens(&new_text);
            }
        }

        Event::ReasoningContent { text } => {
            if text.is_empty() {
                return events;
            }

            // Emit message_start if not yet started
            if !state.started {
                state.started = true;
                events.push(sse_event(
                    "message_start",
                    &json!({
                        "type": "message_start",
                        "message": {
                            "id": state.stream_id,
                            "type": "message",
                            "role": "assistant",
                            "model": model,
                            "usage": {"input_tokens": 0, "output_tokens": 0}
                        }
                    }),
                ));
            }

            // Start thinking block if needed
            if state.current_block_type.as_deref() != Some("thinking") {
                state.stop_current_block(&mut events);
                state.close_open_tool_blocks(&mut events);
                let idx = state.alloc_block_index();
                events.push(sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {"type": "thinking", "thinking": ""}
                    }),
                ));
                state.current_block_type = Some("thinking".to_string());
                state.current_block_index = Some(idx);
            }

            let idx = state.current_block_index.unwrap_or(0);
            events.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": {"type": "thinking_delta", "thinking": text}
                }),
            ));
            state.output_tokens += estimate_tokens(&text);
        }

        Event::ToolUse {
            name,
            tool_use_id,
            input,
            stop,
        } => {
            state.has_tool_use = true;

            // Emit message_start if not yet started
            if !state.started {
                state.started = true;
                events.push(sse_event(
                    "message_start",
                    &json!({
                        "type": "message_start",
                        "message": {
                            "id": state.stream_id,
                            "type": "message",
                            "role": "assistant",
                            "model": model,
                            "usage": {"input_tokens": 0, "output_tokens": 0}
                        }
                    }),
                ));
            }

            // Accumulate input
            let buffer = state
                .tool_input_buffers
                .entry(tool_use_id.clone())
                .or_default();
            buffer.push_str(input);

            // Start new tool block if not yet started
            if !state.tool_block_indices.contains_key(tool_use_id) {
                // Close current text/thinking block
                state.stop_current_block(&mut events);
                state.close_open_tool_blocks(&mut events);

                let idx = state.alloc_block_index();
                let original_name = tool_name_map
                    .get(name.as_str())
                    .map(|s| s.as_str())
                    .unwrap_or(name.as_str());

                events.push(sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {
                            "type": "tool_use",
                            "id": tool_use_id,
                            "name": original_name,
                            "input": {}
                        }
                    }),
                ));
                state.tool_block_indices.insert(tool_use_id.clone(), idx);
                state.open_tool_blocks.push(idx);
            }

            // Emit input delta
            if !input.is_empty() {
                let idx = state.tool_block_indices[tool_use_id];
                events.push(sse_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "input_json_delta", "partial_json": input}
                    }),
                ));
            }

            // Close tool block on stop
            if *stop {
                if let Some(idx) = state.tool_block_indices.get(tool_use_id) {
                    events.push(sse_content_block_stop(*idx));
                    state.open_tool_blocks.retain(|i| *i != *idx);
                }
            }
        }

        Event::ContextUsage { percentage } => {
            let window = context_window_size(model);
            state.input_tokens = (*percentage as f64 * window as f64 / 100.0) as u64;
            if *percentage >= 100.0 {
                state.stop_reason = Some("model_context_window_exceeded".to_string());
            }
        }

        Event::Metering { .. } => {
            // Billing info - not directly mappable to Anthropic usage
        }

        Event::Error { code, message } => {
            warn!(code, message, "Kiro API 错误");
        }

        Event::Exception {
            type_name,
            message,
        } => {
            warn!(type_name, message, "Kiro API 异常");
            if type_name == "ContentLengthExceededException" {
                state.stop_reason = Some("max_tokens".to_string());
            }
        }

        Event::Unknown => {}
    }

    events
}

// ---- Streaming handler ----

/// Handle a Kiro EventStream response, converting to Anthropic SSE format.
pub async fn handle_stream_anthropic_output(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_map: HashMap<String, String>,
    request_id: String,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
    log_ctx: Option<StreamLogContext>,
    thinking_mode: Option<&str>,
) -> Result<Response> {
    info!(
        request_id = request_id.as_str(),
        upstream_headers_ms, "开始处理 Kiro 流式响应，转换为 Anthropic 格式"
    );

    let byte_stream = upstream_resp.bytes_stream();
    let model = model.to_string();
    let thinking_mode_owned: Option<String> = thinking_mode.map(|s| s.to_string());

    // Create channel outside spawn so rx is available for the response body
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, AppError>>(128);
    let keepalive_tx = tx.clone();
    let stream_base = Instant::now();
    let last_data_sent = Arc::new(AtomicU64::new(0));

    // Spawn keep-alive task
    let last_ds = last_data_sent.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(KEEP_ALIVE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let elapsed_since_last = Duration::from_secs(
                stream_base.elapsed().as_secs() - last_ds.load(Ordering::Relaxed),
            );
            if elapsed_since_last >= KEEP_ALIVE_TIMEOUT {
                debug!("SSE keep-alive: stream timed out after {:?} of inactivity", elapsed_since_last);
                break;
            }
            if keepalive_tx.send(Ok(Bytes::from_static(KEEP_ALIVE_BYTES))).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let stream_start = Instant::now();
        let mut decoder = EventStreamDecoder::new();
        let mut state = AnthropicStreamState::new();
        // Initialize ThinkingParser if thinking_mode is configured
        if let Some(ref mode_str) = thinking_mode_owned {
            let mode = ThinkingHandlingMode::from_str(mode_str);
            state.thinking_parser = Some(ThinkingParser::new(mode));
        }
        let mut upstream_chunks: u64 = 0;
        let mut emitted_events: u64 = 0;
        let mut has_emitted_message_delta = false;

        let log_stream_end = |reason: &str, state: &AnthropicStreamState, upstream_chunks: u64, emitted_events: u64| {
            let (status, error_message) = match reason {
                "done" | "eof" => (200, None),
                "client_disconnected" => (499, Some("client disconnected".to_string())),
                "upstream_error" => (502, Some("upstream error".to_string())),
                other => (502, Some(format!("stream ended: {}", other))),
            };
            info!(
                request_id = request_id.as_str(),
                stream_id = state.stream_id.as_str(),
                reason,
                started = state.started,
                ended = state.ended,
                upstream_chunks,
                emitted_events,
                input_tokens = state.input_tokens,
                output_tokens = state.completion_tokens,
                stop_reason = state.get_stop_reason(),
                upstream_headers_ms,
                upstream_total_ms = elapsed_ms(upstream_start),
                stream_total_ms = elapsed_ms(stream_start),
                request_total_ms = elapsed_ms(request_start),
                "Kiro→Anthropic 流式响应结束"
            );
            if let Some(log_ctx) = &log_ctx {
                log_ctx.emit(status, Some(upstream_headers_ms as u64), error_message, None);
            }
        };

        let mut stream = byte_stream;
        let mut first_chunk = true;

        loop {
            let timeout_duration = if first_chunk {
                Duration::from_secs(15) // first-token timeout
            } else {
                KEEP_ALIVE_TIMEOUT // overall stream stall timeout
            };
            let keepalive_timer = tokio::time::sleep(timeout_duration);
            tokio::pin!(keepalive_timer);

            tokio::select! {
                biased;
                result = stream.next() => {
                    match result {
                        Some(Ok(bytes)) => {
                            // Real data received — reset keep-alive tracking
                            last_data_sent.store(stream_base.elapsed().as_secs(), Ordering::Relaxed);
                            first_chunk = false;

                            if let Err(e) = decoder.feed(&bytes) {
                                error!(error = %e, "EventStream feed 错误");
                                let _ = tx.send(Err(AppError::Request(e.to_string()))).await;
                                log_stream_end("upstream_error", &state, upstream_chunks, emitted_events);
                                return;
                            }

                            // Decode all available frames
                            loop {
                                match decoder.decode() {
                                    Ok(Some(frame)) => {
                                        upstream_chunks += 1;
                                        match Event::from_frame(&frame) {
                                            Ok(event) => {
                                                let sse_events = process_event(
                                                    &event,
                                                    &model,
                                                    &mut state,
                                                    &tool_name_map,
                                                );
                                                for evt in sse_events {
                                                    match tx.send(Ok(Bytes::from(evt))).await {
                                                        Ok(_) => emitted_events += 1,
                                                        Err(_) => {
                                                            log_stream_end("client_disconnected", &state, upstream_chunks, emitted_events);
                                                            return;
                                                        }
                                                    }
                                                    if state.ended {
                                                        has_emitted_message_delta = true;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                warn!(error = %e, "Event 解析错误，跳过");
                                            }
                                        }
                                    }
                                    Ok(None) => break, // need more data
                                    Err(e) => {
                                        if decoder.is_stopped() {
                                            error!(error = %e, "EventStream 解码器已停止");
                                            let _ = tx.send(Err(AppError::Request(e.to_string()))).await;
                                            log_stream_end("upstream_error", &state, upstream_chunks, emitted_events);
                                            return;
                                        }
                                        // Recoverable error, continue
                                        break;
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!(
                                request_id = request_id.as_str(),
                                error = %e,
                                "Kiro 流式读取错误"
                            );
                            let _ = tx.send(Err(AppError::Http(e))).await;
                            log_stream_end("upstream_error", &state, upstream_chunks, emitted_events);
                            return;
                        }
                        None => break, // upstream EOF
                    }
                }
                _ = &mut keepalive_timer => {
                    let reason = if first_chunk { "first_token_timeout" } else { "stream_timeout" };
                    warn!("Kiro 流式超时: {}", reason);
                    log_stream_end(reason, &state, upstream_chunks, emitted_events);
                    return;
                }
            }
        }

        // Stream ended — emit final events
        if state.started {
            // Close any open blocks
            state.stop_current_block(&mut vec![]);
            state.close_open_tool_blocks(&mut vec![]);

            // Finalize ThinkingParser to flush remaining buffer
            if let Some(ref mut parser) = state.thinking_parser {
                let final_outputs = parser.finalize();
                for output in final_outputs {
                    match output {
                        ThinkingOutput::ThinkingDelta(text) => {
                            // Emit remaining thinking content
                            if !state.in_thinking {
                                let idx = state.alloc_block_index();
                                let start = sse_event("content_block_start", &json!({
                                    "type": "content_block_start", "index": idx,
                                    "content_block": {"type": "thinking", "thinking": ""}
                                }));
                                let _ = tx.send(Ok(Bytes::from(start))).await;
                                state.in_thinking = true;
                                state.current_block_type = Some("thinking".to_string());
                                state.current_block_index = Some(idx);
                            }
                            let idx = state.current_block_index.unwrap_or(0);
                            let delta = sse_event("content_block_delta", &json!({
                                "type": "content_block_delta", "index": idx,
                                "delta": {"type": "thinking_delta", "thinking": text}
                            }));
                            let _ = tx.send(Ok(Bytes::from(delta))).await;
                        }
                        ThinkingOutput::ContentDelta(text) => {
                            if state.in_thinking {
                                state.stop_current_block(&mut vec![]);
                            }
                            let idx = state.alloc_block_index();
                            let start = sse_event("content_block_start", &json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": {"type": "text", "text": ""}
                            }));
                            let _ = tx.send(Ok(Bytes::from(start))).await;
                            let delta = sse_event("content_block_delta", &json!({
                                "type": "content_block_delta", "index": idx,
                                "delta": {"type": "text_delta", "text": text}
                            }));
                            let _ = tx.send(Ok(Bytes::from(delta))).await;
                            let stop = sse_content_block_stop(idx);
                            let _ = tx.send(Ok(Bytes::from(stop))).await;
                        }
                        ThinkingOutput::None => {}
                    }
                }
            }

            // Emit message_delta with stop_reason and usage
            if !has_emitted_message_delta {
                // Truncation detection: if no usage event was received, the stream may be truncated
                if state.input_tokens == 0 && state.output_tokens > 0 {
                    warn!(
                        request_id = request_id.as_str(),
                        "检测到可能的流截断: 未收到 contextUsage 事件"
                    );
                }

                let stop_reason = state.get_stop_reason().to_string();
                let delta = sse_event(
                    "message_delta",
                    &json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                        "usage": {"output_tokens": state.output_tokens}
                    }),
                );
                let _ = tx.send(Ok(Bytes::from(delta))).await;
                emitted_events += 1;
            }

            // Emit message_stop
            let msg_stop = sse_event_literal("message_stop", r#"{"type":"message_stop"}"#);
            let _ = tx.send(Ok(Bytes::from(msg_stop))).await;
            emitted_events += 1;

            log_stream_end("done", &state, upstream_chunks, emitted_events);
        } else {
            log_stream_end("eof", &state, upstream_chunks, emitted_events);
        }
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

// ---- OpenAI SSE output ----

/// State machine for converting Kiro events to OpenAI SSE chunks.
struct OpenAiStreamState {
    stream_id: String,
    started: bool,
    ended: bool,
    has_role_emitted: bool,
    has_tool_use: bool,
    tool_call_counter: usize,
    tool_block_indices: HashMap<String, usize>, // tool_use_id → OpenAI tool_call index
    stop_reason: Option<String>,
    output_tokens: usize,
    input_tokens: u64,
    last_content: String,
}

impl OpenAiStreamState {
    fn new() -> Self {
        Self {
            stream_id: format!("chatcmpl-{}", generate_id()),
            started: false,
            ended: false,
            has_role_emitted: false,
            has_tool_use: false,
            tool_call_counter: 0,
            tool_block_indices: HashMap::new(),
            stop_reason: None,
            output_tokens: 0,
            input_tokens: 0,
            last_content: String::new(),
        }
    }

    fn get_finish_reason(&self) -> Option<&str> {
        if let Some(sr) = &self.stop_reason {
            return Some(match sr.as_str() {
                "model_context_window_exceeded" | "max_tokens" => "length",
                _ => "stop",
            });
        }
        if self.has_tool_use {
            Some("tool_calls")
        } else {
            None
        }
    }
}

fn openai_sse_chunk(data: &Value) -> String {
    let json_str = serde_json::to_string(data).unwrap_or_default();
    let mut s = String::with_capacity(6 + json_str.len() + 2);
    s.push_str("data: ");
    s.push_str(&json_str);
    s.push('\n');
    s.push('\n');
    s
}

fn process_event_openai(
    event: &Event,
    model: &str,
    state: &mut OpenAiStreamState,
    tool_name_map: &HashMap<String, String>,
) -> Vec<String> {
    let mut events = Vec::new();

    match event {
        Event::AssistantResponse { content } => {
            if content.is_empty() {
                return events;
            }

            // Text dedup
            if state.last_content.len() >= content.len()
                && state.last_content[..content.len()] == *content
            {
                return events;
            }
            let new_text = if content.starts_with(&state.last_content)
                && !state.last_content.is_empty()
            {
                &content[state.last_content.len()..]
            } else {
                content.as_str()
            };
            state.last_content = content.clone();

            if new_text.is_empty() {
                return events;
            }

            // First chunk with role
            if !state.has_role_emitted {
                state.has_role_emitted = true;
                state.started = true;
                events.push(openai_sse_chunk(&json!({
                    "id": state.stream_id,
                    "object": "chat.completion.chunk",
                    "created": now_epoch_secs(),
                    "model": model,
                    "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
                })));
            }

            events.push(openai_sse_chunk(&json!({
                "id": state.stream_id,
                "object": "chat.completion.chunk",
                "created": now_epoch_secs(),
                "model": model,
                "choices": [{"index": 0, "delta": {"content": new_text}, "finish_reason": null}]
            })));
            state.output_tokens += estimate_tokens(&new_text);
        }

        Event::ReasoningContent { text } => {
            if text.is_empty() {
                return events;
            }

            if !state.has_role_emitted {
                state.has_role_emitted = true;
                state.started = true;
                events.push(openai_sse_chunk(&json!({
                    "id": state.stream_id,
                    "object": "chat.completion.chunk",
                    "created": now_epoch_secs(),
                    "model": model,
                    "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
                })));
            }

            events.push(openai_sse_chunk(&json!({
                "id": state.stream_id,
                "object": "chat.completion.chunk",
                "created": now_epoch_secs(),
                "model": model,
                "choices": [{"index": 0, "delta": {"reasoning_content": text}, "finish_reason": null}]
            })));
            state.output_tokens += estimate_tokens(&text);
        }

        Event::ToolUse {
            name,
            tool_use_id,
            input,
            stop,
        } => {
            state.has_tool_use = true;

            if !state.has_role_emitted {
                state.has_role_emitted = true;
                state.started = true;
                events.push(openai_sse_chunk(&json!({
                    "id": state.stream_id,
                    "object": "chat.completion.chunk",
                    "created": now_epoch_secs(),
                    "model": model,
                    "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
                })));
            }

            // New tool call
            if !state.tool_block_indices.contains_key(tool_use_id) {
                let index = state.tool_call_counter;
                state.tool_call_counter += 1;
                state.tool_block_indices.insert(tool_use_id.clone(), index);

                let original_name = tool_name_map
                    .get(name.as_str())
                    .map(|s| s.as_str())
                    .unwrap_or(name.as_str());
                let openai_id = format!("call_{}", &tool_use_id[tool_use_id.len().min(6)..]);

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
                                "id": openai_id,
                                "type": "function",
                                "function": {"name": original_name, "arguments": ""}
                            }]
                        },
                        "finish_reason": null
                    }]
                })));
            }

            // Input delta
            if !input.is_empty() {
                let index = state.tool_block_indices[tool_use_id];
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
                                "function": {"arguments": input}
                            }]
                        },
                        "finish_reason": null
                    }]
                })));
            }
        }

        Event::ContextUsage { percentage } => {
            let window = context_window_size(model);
            state.input_tokens = (*percentage as f64 * window as f64 / 100.0) as u64;
            if *percentage >= 100.0 {
                state.stop_reason = Some("model_context_window_exceeded".to_string());
            }
        }

        Event::Exception {
            type_name, ..
        } => {
            if type_name == "ContentLengthExceededException" {
                state.stop_reason = Some("max_tokens".to_string());
            }
        }

        _ => {}
    }

    events
}

/// Handle a Kiro EventStream response, converting to OpenAI SSE format.
pub async fn handle_stream_openai_output(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_map: HashMap<String, String>,
    request_id: String,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
    log_ctx: Option<StreamLogContext>,
    thinking_mode: Option<&str>,
) -> Result<Response> {
    info!(
        request_id = request_id.as_str(),
        upstream_headers_ms, "开始处理 Kiro 流式响应，转换为 OpenAI 格式"
    );

    let byte_stream = upstream_resp.bytes_stream();
    let model = model.to_string();

    // Create channel outside spawn so rx is available for the response body
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, AppError>>(128);
    let keepalive_tx = tx.clone();
    let stream_base = Instant::now();
    let last_data_sent = Arc::new(AtomicU64::new(0));

    // Spawn keep-alive task
    let last_ds = last_data_sent.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(KEEP_ALIVE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let elapsed_since_last = Duration::from_secs(
                stream_base.elapsed().as_secs() - last_ds.load(Ordering::Relaxed),
            );
            if elapsed_since_last >= KEEP_ALIVE_TIMEOUT {
                debug!("SSE keep-alive: stream timed out after {:?} of inactivity", elapsed_since_last);
                break;
            }
            if keepalive_tx.send(Ok(Bytes::from_static(KEEP_ALIVE_BYTES))).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let stream_start = Instant::now();
        let mut decoder = EventStreamDecoder::new();
        let mut state = OpenAiStreamState::new();
        let mut upstream_chunks: u64 = 0;
        let mut emitted_events: u64 = 0;

        let log_stream_end = |reason: &str, state: &OpenAiStreamState, upstream_chunks: u64, emitted_events: u64| {
            let (status, error_message) = match reason {
                "done" | "eof" => (200, None),
                "client_disconnected" => (499, Some("client disconnected".to_string())),
                "upstream_error" => (502, Some("upstream error".to_string())),
                other => (502, Some(format!("stream ended: {}", other))),
            };
            info!(
                request_id = request_id.as_str(),
                stream_id = state.stream_id.as_str(),
                reason,
                started = state.started,
                ended = state.ended,
                upstream_chunks,
                emitted_events,
                input_tokens = state.input_tokens,
                upstream_headers_ms,
                upstream_total_ms = elapsed_ms(upstream_start),
                stream_total_ms = elapsed_ms(stream_start),
                request_total_ms = elapsed_ms(request_start),
                "Kiro→OpenAI 流式响应结束"
            );
            if let Some(log_ctx) = &log_ctx {
                log_ctx.emit(status, Some(upstream_headers_ms as u64), error_message, None);
            }
        };

        let mut stream = byte_stream;
        let mut first_chunk = true;

        loop {
            let timeout_duration = if first_chunk {
                Duration::from_secs(15) // first-token timeout
            } else {
                KEEP_ALIVE_TIMEOUT // overall stream stall timeout
            };
            let keepalive_timer = tokio::time::sleep(timeout_duration);
            tokio::pin!(keepalive_timer);

            tokio::select! {
                biased;
                result = stream.next() => {
                    match result {
                        Some(Ok(bytes)) => {
                            // Real data received — reset keep-alive tracking
                            last_data_sent.store(stream_base.elapsed().as_secs(), Ordering::Relaxed);
                            first_chunk = false;

                            if let Err(e) = decoder.feed(&bytes) {
                                error!(error = %e, "EventStream feed 错误");
                                let _ = tx.send(Err(AppError::Request(e.to_string()))).await;
                                log_stream_end("upstream_error", &state, upstream_chunks, emitted_events);
                                return;
                            }

                            loop {
                                match decoder.decode() {
                                    Ok(Some(frame)) => {
                                        upstream_chunks += 1;
                                        match Event::from_frame(&frame) {
                                            Ok(event) => {
                                                let sse_events = process_event_openai(
                                                    &event,
                                                    &model,
                                                    &mut state,
                                                    &tool_name_map,
                                                );
                                                for evt in sse_events {
                                                    match tx.send(Ok(Bytes::from(evt))).await {
                                                        Ok(_) => emitted_events += 1,
                                                        Err(_) => {
                                                            log_stream_end("client_disconnected", &state, upstream_chunks, emitted_events);
                                                            return;
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                warn!(error = %e, "Event 解析错误，跳过");
                                            }
                                        }
                                    }
                                    Ok(None) => break,
                                    Err(e) => {
                                        if decoder.is_stopped() {
                                            error!(error = %e, "EventStream 解码器已停止");
                                            let _ = tx.send(Err(AppError::Request(e.to_string()))).await;
                                            log_stream_end("upstream_error", &state, upstream_chunks, emitted_events);
                                            return;
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!(request_id = request_id.as_str(), error = %e, "Kiro 流式读取错误");
                            let _ = tx.send(Err(AppError::Http(e))).await;
                            log_stream_end("upstream_error", &state, upstream_chunks, emitted_events);
                            return;
                        }
                        None => break, // upstream EOF
                    }
                }
                _ = &mut keepalive_timer => {
                    let reason = if first_chunk { "first_token_timeout" } else { "stream_timeout" };
                    warn!("Kiro 流式超时: {}", reason);
                    log_stream_end(reason, &state, upstream_chunks, emitted_events);
                    return;
                }
            }
        }

        // Final chunk with finish_reason and usage
        if state.started {
            let finish_reason = state.get_finish_reason();
            let final_chunk = openai_sse_chunk(&json!({
                "id": state.stream_id,
                "object": "chat.completion.chunk",
                "created": now_epoch_secs(),
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish_reason
                }],
                "usage": {
                    "prompt_tokens": state.input_tokens,
                    "completion_tokens": state.output_tokens,
                    "total_tokens": state.input_tokens + state.output_tokens as u64
                }
            }));
            let _ = tx.send(Ok(Bytes::from(final_chunk))).await;
            emitted_events += 1;

            // [DONE]
            let done = "data: [DONE]\n\n";
            let _ = tx.send(Ok(Bytes::from(done.to_string()))).await;
            emitted_events += 1;

            log_stream_end("done", &state, upstream_chunks, emitted_events);
        } else {
            log_stream_end("eof", &state, upstream_chunks, emitted_events);
        }
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

// ---- Helpers ----

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:016x}", t)
}

/// Estimate token count from text using word-count heuristic.
/// More accurate than `text.len() / 4` for mixed CJK/Latin text.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let words = text.split_whitespace().count();
    let punctuation = text.chars().filter(|c| c.is_ascii_punctuation()).count();
    ((words + punctuation) as f64 * 1.3) as usize + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_dedup_skips_prefix() {
        let mut state = AnthropicStreamState::new();
        state.started = true;
        state.current_block_type = Some("text".to_string());
        state.current_block_index = Some(0);

        // First chunk
        state.last_content = "Hello".to_string();

        // Second chunk is cumulative: "Hello World"
        let event = Event::AssistantResponse {
            content: "Hello World".to_string(),
        };
        let events = process_event(&event, "test-model", &mut state, &HashMap::new());
        // Should only emit " World", not "Hello World"
        let has_world_only = events.iter().any(|e| e.contains("World") && !e.contains("Hello World"));
        // The delta should contain just the new part
        assert!(state.last_content == "Hello World");
    }

    #[test]
    fn tool_use_creates_block() {
        let mut state = AnthropicStreamState::new();
        state.started = true;

        let event = Event::ToolUse {
            name: "search".to_string(),
            tool_use_id: "toolu_abc123".to_string(),
            input: r#"{"query":"test"}"#.to_string(),
            stop: true,
        };
        let events = process_event(&event, "test-model", &mut state, &HashMap::new());

        // Should have content_block_start + content_block_delta + content_block_stop
        assert!(events.len() >= 3);
        assert!(events[0].contains("content_block_start"));
        assert!(events[0].contains("tool_use"));
        assert!(events[0].contains("search"));
        assert!(state.has_tool_use);
    }

    #[test]
    fn context_usage_updates_tokens() {
        let mut state = AnthropicStreamState::new();
        let event = Event::ContextUsage { percentage: 50.0 };
        process_event(&event, "claude-sonnet-4.5", &mut state, &HashMap::new());
        assert_eq!(state.input_tokens, 100_000); // 50% of 200K
    }

    #[test]
    fn context_usage_100_sets_stop_reason() {
        let mut state = AnthropicStreamState::new();
        let event = Event::ContextUsage { percentage: 100.0 };
        process_event(&event, "claude-sonnet-4.5", &mut state, &HashMap::new());
        assert_eq!(
            state.stop_reason.as_deref(),
            Some("model_context_window_exceeded")
        );
    }

    #[test]
    fn stop_reason_tool_use() {
        let mut state = AnthropicStreamState::new();
        state.has_tool_use = true;
        assert_eq!(state.get_stop_reason(), "tool_use");
    }

    #[test]
    fn stop_reason_default_end_turn() {
        let state = AnthropicStreamState::new();
        assert_eq!(state.get_stop_reason(), "end_turn");
    }
}
