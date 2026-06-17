//! Kiro EventStream → OpenAI SSE streaming conversion.
//!
//! Converts AWS EventStream binary frames from the Kiro API into
//! OpenAI Chat Completions SSE format (data: {...}\ndata: [DONE]).

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
use super::thinking_parser::{ThinkingHandlingMode, ThinkingOutput, ThinkingParser};
use crate::convert::anthropic_openai::stream::StreamLogContext;
use crate::error::{AppError, Result};
use crate::server::state::elapsed_ms;

use super::stream::{generate_id, estimate_tokens, KEEP_ALIVE_BYTES};
use super::model_map::context_window_size;

// ---- Helpers ----

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
    thinking_parser: Option<ThinkingParser>,
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
            thinking_parser: None,
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

            // Ensure role is emitted first
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

            // Use ThinkingParser if configured, otherwise emit directly
            if let Some(ref mut parser) = state.thinking_parser {
                let outputs = parser.feed(new_text);
                for output in outputs {
                    match output {
                        ThinkingOutput::ThinkingDelta(thinking_text) => {
                            events.push(openai_sse_chunk(&json!({
                                "id": state.stream_id,
                                "object": "chat.completion.chunk",
                                "created": now_epoch_secs(),
                                "model": model,
                                "choices": [{"index": 0, "delta": {"reasoning_content": thinking_text}, "finish_reason": null}]
                            })));
                        }
                        ThinkingOutput::ContentDelta(text) => {
                            events.push(openai_sse_chunk(&json!({
                                "id": state.stream_id,
                                "object": "chat.completion.chunk",
                                "created": now_epoch_secs(),
                                "model": model,
                                "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                            })));
                            state.output_tokens += estimate_tokens(&text);
                        }
                        ThinkingOutput::None => {}
                    }
                }
            } else {
                events.push(openai_sse_chunk(&json!({
                    "id": state.stream_id,
                    "object": "chat.completion.chunk",
                    "created": now_epoch_secs(),
                    "model": model,
                    "choices": [{"index": 0, "delta": {"content": new_text}, "finish_reason": null}]
                })));
                state.output_tokens += estimate_tokens(new_text);
            }
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
            state.output_tokens += estimate_tokens(text);
        }

        Event::ToolUse {
            name,
            tool_use_id,
            input,
            stop: _,
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
            state.input_tokens = (*percentage * window as f64 / 100.0) as u64;
            if *percentage >= 100.0 {
                state.stop_reason = Some("model_context_window_exceeded".to_string());
            }
        }

        Event::Exception {
            type_name, ..
        }
            if type_name == "ContentLengthExceededException" => {
                state.stop_reason = Some("max_tokens".to_string());
            }

        _ => {}
    }

    events
}

/// Handle a Kiro EventStream response, converting to OpenAI SSE format.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_stream_openai_output(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_map: HashMap<String, String>,
    request_id: String,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
    log_ctx: Option<StreamLogContext>,
    thinking_mode: Option<&str>,
    first_token_timeout: Duration,
    streaming_read_timeout: Duration,
) -> Result<Response> {
    info!(
        request_id = request_id.as_str(),
        upstream_headers_ms, "开始处理 Kiro 流式响应，转换为 OpenAI 格式"
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
    let keepalive_interval = streaming_read_timeout / 12;
    let keepalive_timeout = streaming_read_timeout;
    let last_ds = last_data_sent.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(keepalive_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let elapsed_since_last = Duration::from_secs(
                stream_base.elapsed().as_secs().saturating_sub(last_ds.load(Ordering::Relaxed)),
            );
            if elapsed_since_last >= keepalive_timeout {
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
        // Initialize ThinkingParser for extracting thinking tags from assistant responses
        if let Some(ref mode_str) = thinking_mode_owned {
            let mode = ThinkingHandlingMode::from_str(mode_str);
            state.thinking_parser = Some(ThinkingParser::new(mode));
        }
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
                first_token_timeout
            } else {
                streaming_read_timeout
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
                    // Send final chunk + [DONE] so client doesn't hang
                    if state.started {
                        let finish_reason = if first_chunk { "error" } else { state.get_finish_reason().unwrap_or("stop") };
                        let final_chunk = openai_sse_chunk(&json!({
                            "id": state.stream_id,
                            "object": "chat.completion.chunk",
                            "created": now_epoch_secs(),
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": {},
                                "finish_reason": finish_reason,
                            }],
                            "usage": {
                                "prompt_tokens": state.input_tokens,
                                "completion_tokens": state.output_tokens as u64,
                                "total_tokens": state.input_tokens + state.output_tokens as u64,
                            }
                        }));
                        let _ = tx.send(Ok(Bytes::from(final_chunk))).await;
                        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                    }
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


#[cfg(test)]
mod tests {
    use super::*;

    // ---- OpenAiStreamState tests ----

    #[test]
    fn finish_reason_default_none() {
        let state = OpenAiStreamState::new();
        assert_eq!(state.get_finish_reason(), None);
    }

    #[test]
    fn finish_reason_tool_use() {
        let mut state = OpenAiStreamState::new();
        state.has_tool_use = true;
        assert_eq!(state.get_finish_reason(), Some("tool_calls"));
    }

    #[test]
    fn finish_reason_max_tokens() {
        let mut state = OpenAiStreamState::new();
        state.stop_reason = Some("max_tokens".to_string());
        assert_eq!(state.get_finish_reason(), Some("length"));
    }

    #[test]
    fn finish_reason_context_exceeded() {
        let mut state = OpenAiStreamState::new();
        state.stop_reason = Some("model_context_window_exceeded".to_string());
        assert_eq!(state.get_finish_reason(), Some("length"));
    }

    #[test]
    fn finish_reason_explicit_stop_overrides_tool_use() {
        let mut state = OpenAiStreamState::new();
        state.has_tool_use = true;
        state.stop_reason = Some("max_tokens".to_string());
        // stop_reason takes priority over has_tool_use
        assert_eq!(state.get_finish_reason(), Some("length"));
    }

    // ---- openai_sse_chunk formatting ----

    #[test]
    fn openai_sse_chunk_format() {
        let data = json!({"id": "test", "object": "chat.completion.chunk"});
        let chunk = openai_sse_chunk(&data);
        assert!(chunk.starts_with("data: "));
        assert!(chunk.ends_with("\n\n"));
        assert!(chunk.contains("\"id\":\"test\""));
    }

    // ---- process_event_openai tests ----

    #[test]
    fn assistant_response_emits_role_first() {
        let mut state = OpenAiStreamState::new();
        let event = Event::AssistantResponse {
            content: "Hello".to_string(),
        };
        let events = process_event_openai(&event, "test-model", &mut state, &HashMap::new());

        // First event should be role
        assert!(events.len() >= 2);
        assert!(events[0].contains("\"role\":\"assistant\""));
        // Second event should be content
        assert!(events[1].contains("\"content\":\"Hello\""));
        assert!(state.started);
        assert!(state.has_role_emitted);
    }

    #[test]
    fn assistant_response_dedup_cumulative_text() {
        let mut state = OpenAiStreamState::new();
        state.has_role_emitted = true;
        state.started = true;

        // First chunk: "Hello"
        let event1 = Event::AssistantResponse {
            content: "Hello".to_string(),
        };
        let events1 = process_event_openai(&event1, "model", &mut state, &HashMap::new());
        assert!(!events1.is_empty());

        // Second chunk: "Hello World" (cumulative)
        let event2 = Event::AssistantResponse {
            content: "Hello World".to_string(),
        };
        let events2 = process_event_openai(&event2, "model", &mut state, &HashMap::new());

        // Should only emit " World"
        assert!(!events2.is_empty());
        assert!(events2[0].contains(" World"));
        assert!(!events2[0].contains("Hello World"));
    }

    #[test]
    fn assistant_response_skip_prefix() {
        let mut state = OpenAiStreamState::new();
        state.has_role_emitted = true;
        state.started = true;
        state.last_content = "Hello World".to_string();

        // Chunk is a prefix of what we've already seen → skip
        let event = Event::AssistantResponse {
            content: "Hello".to_string(),
        };
        let events = process_event_openai(&event, "model", &mut state, &HashMap::new());
        assert!(events.is_empty());
    }

    #[test]
    fn empty_content_skipped() {
        let mut state = OpenAiStreamState::new();
        let event = Event::AssistantResponse {
            content: String::new(),
        };
        let events = process_event_openai(&event, "model", &mut state, &HashMap::new());
        assert!(events.is_empty());
    }

    #[test]
    fn reasoning_content_emits_role_and_delta() {
        let mut state = OpenAiStreamState::new();
        let event = Event::ReasoningContent {
            text: "thinking...".to_string(),
        };
        let events = process_event_openai(&event, "model", &mut state, &HashMap::new());

        assert!(events.len() >= 2);
        assert!(events[0].contains("\"role\":\"assistant\""));
        assert!(events[1].contains("\"reasoning_content\":\"thinking...\""));
        assert!(state.started);
    }

    #[test]
    fn tool_use_creates_tool_call() {
        let mut state = OpenAiStreamState::new();
        state.has_role_emitted = true;
        state.started = true;

        let event = Event::ToolUse {
            name: "search".to_string(),
            tool_use_id: "toolu_abc123def".to_string(),
            input: r#"{"q":"test"}"#.to_string(),
            stop: false,
        };
        let events = process_event_openai(&event, "model", &mut state, &HashMap::new());

        // Should have tool_call start + input delta
        assert!(events.len() >= 2);
        assert!(events[0].contains("\"name\":\"search\""));
        assert!(events[0].contains("\"type\":\"function\""));
        assert!(events[1].contains("\"arguments\""));
        assert!(state.has_tool_use);
        assert_eq!(state.tool_call_counter, 1);
    }

    #[test]
    fn tool_use_with_name_map() {
        let mut state = OpenAiStreamState::new();
        state.has_role_emitted = true;
        state.started = true;

        let mut tool_map = HashMap::new();
        tool_map.insert("short_name".to_string(), "original_very_long_tool_name".to_string());

        let event = Event::ToolUse {
            name: "short_name".to_string(),
            tool_use_id: "toolu_xyz".to_string(),
            input: "{}".to_string(),
            stop: true,
        };
        let events = process_event_openai(&event, "model", &mut state, &tool_map);

        // Should use the original name
        assert!(events[0].contains("original_very_long_tool_name"));
    }

    #[test]
    fn context_usage_updates_tokens() {
        let mut state = OpenAiStreamState::new();
        let event = Event::ContextUsage { percentage: 50.0 };
        process_event_openai(&event, "claude-sonnet-4.5", &mut state, &HashMap::new());
        assert_eq!(state.input_tokens, 100_000); // 50% of 200K window
    }

    #[test]
    fn context_usage_100_sets_stop_reason() {
        let mut state = OpenAiStreamState::new();
        let event = Event::ContextUsage { percentage: 100.0 };
        process_event_openai(&event, "claude-sonnet-4.5", &mut state, &HashMap::new());
        assert_eq!(
            state.stop_reason.as_deref(),
            Some("model_context_window_exceeded")
        );
    }

    #[test]
    fn exception_content_length_sets_max_tokens() {
        let mut state = OpenAiStreamState::new();
        let event = Event::Exception {
            type_name: "ContentLengthExceededException".to_string(),
            message: "too long".to_string(),
        };
        process_event_openai(&event, "model", &mut state, &HashMap::new());
        assert_eq!(state.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn multiple_tool_calls_get_different_indices() {
        let mut state = OpenAiStreamState::new();
        state.has_role_emitted = true;
        state.started = true;

        let event1 = Event::ToolUse {
            name: "tool_a".to_string(),
            tool_use_id: "toolu_aaa".to_string(),
            input: "{}".to_string(),
            stop: true,
        };
        process_event_openai(&event1, "model", &mut state, &HashMap::new());

        let event2 = Event::ToolUse {
            name: "tool_b".to_string(),
            tool_use_id: "toolu_bbb".to_string(),
            input: "{}".to_string(),
            stop: true,
        };
        process_event_openai(&event2, "model", &mut state, &HashMap::new());

        assert_eq!(state.tool_call_counter, 2);
        assert_eq!(state.tool_block_indices["toolu_aaa"], 0);
        assert_eq!(state.tool_block_indices["toolu_bbb"], 1);
    }
}
