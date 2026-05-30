use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use smallvec::SmallVec;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{error, info};

use super::request::{anthropic_id_to_openai, openai_id_to_anthropic};
use crate::server::state::elapsed_ms;
use crate::convert::utils::{append_utf8_safe, find_sse_block_end};
use crate::error::{AppError, Result};
use crate::logging::{truncate_body, LogCollector, LogEntry};

#[derive(Clone)]
pub(crate) struct StreamLogContext {
    pub(crate) collector: Arc<LogCollector>,
    pub(crate) request_id: String,
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) requested_model: String,
    pub(crate) request_start: Instant,
    pub(crate) upstream_start: Instant,
    pub(crate) raw_request_body: String,
}

impl StreamLogContext {
    pub(crate) fn emit(
        &self,
        status: u16,
        ttft_ms: Option<u64>,
        error_message: Option<String>,
        response_body: Option<&str>,
    ) {
        if !self.collector.should_log(status) {
            return;
        }

        let log_config = self.collector.config.load();
        let entry = LogEntry {
            id: self.request_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            method: self.method.to_string(),
            path: self.path.to_string(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            requested_model: Some(self.requested_model.clone()),
            status,
            duration_ms: elapsed_ms(self.request_start) as u64,
            proxy_overhead_ms: Some(
                self.upstream_start
                    .duration_since(self.request_start)
                    .as_millis() as u64,
            ),
            ttft_ms,
            error_message,
            request_body: if log_config.record_body {
                Some(truncate_body(
                    &self.raw_request_body,
                    log_config.max_body_bytes,
                ))
            } else {
                None
            },
            response_body: if log_config.record_body {
                response_body.map(|body| truncate_body(body, log_config.max_body_bytes))
            } else {
                None
            },
            is_stream: true,
            token_count: None,
        };
        self.collector.emit(entry);
    }
}

// ---- SSE formatting helpers (hot path) ----
// These helpers centralize SSE framing, pre-allocate the output string, and serialize
// payloads compactly. Static payloads can use `sse_event_literal` to skip serde_json.

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

/// For tiny, static-structure payloads we skip `serde_json` entirely.
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
    let index = index.to_string();
    let json_len = r#"{"type":"content_block_stop","index":}"#.len() + index.len();
    let mut s = String::with_capacity(8 + "content_block_stop".len() + 8 + json_len + 2);
    s.push_str("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":");
    s.push_str(&index);
    s.push_str("}\n\n");
    s
}

type StreamEvents = SmallVec<[String; 4]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamBlockKind {
    Thinking,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CurrentBlock {
    pub(crate) kind: StreamBlockKind,
    pub(crate) index: usize,
}

/// Mutable state maintained across stream chunks during OpenAI → Anthropic conversion.
pub(crate) struct StreamConversionState {
    pub(crate) stream_id: String,
    pub(crate) started: bool,
    pub(crate) current_block: Option<CurrentBlock>,
    pub(crate) next_content_block_index: usize,
    pub(crate) ended: bool,
    pub(crate) pending_message_delta: Option<String>,
    pub(crate) output_tokens: usize,
    pub(crate) tool_block_indices: HashMap<usize, usize>,
    pub(crate) open_tool_blocks: BTreeSet<usize>,
    pub(crate) stop_reason_value: Option<String>,
}

impl StreamConversionState {
    pub(crate) fn new() -> Self {
        Self {
            stream_id: String::new(),
            started: false,
            current_block: None,
            next_content_block_index: 0,
            ended: false,
            pending_message_delta: None,
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            open_tool_blocks: BTreeSet::new(),
            stop_reason_value: None,
        }
    }
}

fn next_stream_block_index(next_content_block_index: &mut usize) -> usize {
    let index = *next_content_block_index;
    *next_content_block_index += 1;
    index
}

pub(crate) fn next_sse_block<'a>(buffer: &'a str, read_offset: &mut usize) -> Option<&'a str> {
    let search = &buffer[*read_offset..];
    let (pos, delimiter_len) = find_sse_block_end(search)?;
    let block_start = *read_offset;
    let block_end = block_start + pos;
    *read_offset = block_end + delimiter_len;
    Some(&buffer[block_start..block_end])
}

pub(crate) fn compact_sse_buffer(buffer: &mut String, read_offset: &mut usize) {
    if *read_offset > 8192 && *read_offset > buffer.len() / 2 {
        buffer.drain(..*read_offset);
        *read_offset = 0;
    }
}

fn push_content_block_stop(events: &mut StreamEvents, index: usize) {
    events.push(sse_content_block_stop(index));
}

fn stop_current_stream_block(events: &mut StreamEvents, current_block: &mut Option<CurrentBlock>) {
    if let Some(block) = current_block.take() {
        push_content_block_stop(events, block.index);
    }
}

fn close_open_tool_blocks(events: &mut StreamEvents, open_tool_blocks: &mut BTreeSet<usize>) {
    for index in std::mem::take(open_tool_blocks) {
        push_content_block_stop(events, index);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UsageParts {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cache_read_input_tokens: Option<u64>,
    pub(crate) cache_creation_input_tokens: Option<u64>,
}

impl UsageParts {
    pub(crate) fn has_any(self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_input_tokens.is_some()
            || self.cache_creation_input_tokens.is_some()
    }

    pub(crate) fn merge(&mut self, other: UsageParts) {
        if other.input_tokens.is_some() {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens.is_some() {
            self.output_tokens = other.output_tokens;
        }
        if other.cache_read_input_tokens.is_some() {
            self.cache_read_input_tokens = other.cache_read_input_tokens;
        }
        if other.cache_creation_input_tokens.is_some() {
            self.cache_creation_input_tokens = other.cache_creation_input_tokens;
        }
    }
}

fn usage_u64(usage: &Value, key: &str) -> Option<u64> {
    usage.get(key).and_then(|v| v.as_u64())
}

fn usage_nested_u64(usage: &Value, object_key: &str, key: &str) -> Option<u64> {
    usage
        .get(object_key)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_u64())
}

pub(crate) fn extract_openai_usage_parts(usage: &Value) -> UsageParts {
    UsageParts {
        input_tokens: usage_u64(usage, "prompt_tokens")
            .or_else(|| usage_u64(usage, "input_tokens")),
        output_tokens: usage_u64(usage, "completion_tokens")
            .or_else(|| usage_u64(usage, "output_tokens")),
        cache_read_input_tokens: usage_u64(usage, "prompt_cache_hit_tokens")
            .or_else(|| usage_u64(usage, "cache_read_input_tokens"))
            .or_else(|| usage_nested_u64(usage, "prompt_tokens_details", "cached_tokens")),
        cache_creation_input_tokens: usage_u64(usage, "prompt_cache_miss_tokens")
            .or_else(|| usage_u64(usage, "cache_creation_input_tokens"))
            .or_else(|| {
                usage_nested_u64(
                    usage,
                    "prompt_tokens_details",
                    "cache_creation_input_tokens",
                )
            }),
    }
}

pub(crate) fn build_anthropic_usage(parts: UsageParts, fallback_output_tokens: u64) -> Value {
    let mut usage = json!({
        "output_tokens": parts.output_tokens.unwrap_or(fallback_output_tokens)
    });

    if let Some(input_tokens) = parts.input_tokens {
        usage["input_tokens"] = json!(input_tokens);
    }
    if let Some(cache_read) = parts.cache_read_input_tokens {
        usage["cache_read_input_tokens"] = json!(cache_read);
    }
    if let Some(cache_creation) = parts.cache_creation_input_tokens {
        usage["cache_creation_input_tokens"] = json!(cache_creation);
    }

    usage
}

async fn send_final_events(
    tx: &mpsc::Sender<std::result::Result<Bytes, AppError>>,
    current_block: &Option<CurrentBlock>,
    open_tool_blocks: &BTreeSet<usize>,
) -> u64 {
    let mut sent = 0;
    if let Some(block) = *current_block {
        if tx
            .send(Ok(Bytes::from(sse_content_block_stop(block.index))))
            .await
            .is_ok()
        {
            sent += 1;
        }
    }

    for index in open_tool_blocks {
        if tx
            .send(Ok(Bytes::from(sse_content_block_stop(*index))))
            .await
            .is_ok()
        {
            sent += 1;
        }
    }

    sent
}

async fn send_message_stop(tx: &mpsc::Sender<std::result::Result<Bytes, AppError>>) -> bool {
    let msg_stop = sse_event_literal("message_stop", r#"{"type":"message_stop"}"#);
    tx.send(Ok(Bytes::from(msg_stop))).await.is_ok()
}

pub(crate) fn convert_stream_chunk(
    chunk: &Value,
    model: &str,
    state: &mut StreamConversionState,
    tool_name_reverse_map: &HashMap<String, String>,
) -> StreamEvents {
    let mut events = StreamEvents::new();

    let id = chunk
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if !id.is_empty() && state.stream_id.is_empty() {
        state.stream_id = id;
    }

    let choice = chunk
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .cloned()
        .unwrap_or(json!({}));

    let delta = choice.get("delta").cloned().unwrap_or(json!({}));
    let content = delta.get("content").and_then(|v| v.as_str());
    let reasoning = delta.get("reasoning_content").and_then(|v| v.as_str());
    let finish_reason = choice.get("finish_reason").and_then(|v| v.as_str());

    if !state.started && delta.get("role").is_some() {
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
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0
                    }
                }
            }),
        ));
    }

    if let Some(text) = reasoning {
        if !text.is_empty() {
            close_open_tool_blocks(&mut events, &mut state.open_tool_blocks);
            if state.current_block.map(|block| block.kind) != Some(StreamBlockKind::Thinking) {
                stop_current_stream_block(&mut events, &mut state.current_block);
                let index = next_stream_block_index(&mut state.next_content_block_index);
                events.push(sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "thinking",
                            "thinking": ""
                        }
                    }),
                ));
                state.current_block = Some(CurrentBlock {
                    kind: StreamBlockKind::Thinking,
                    index,
                });
            }
            let index = state.current_block.map(|block| block.index).unwrap_or(0);
            events.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "thinking_delta",
                        "thinking": text
                    }
                }),
            ));
            state.output_tokens += text.len() / 4 + 1;
        }
    }

    if let Some(text) = content {
        if !text.is_empty() {
            close_open_tool_blocks(&mut events, &mut state.open_tool_blocks);
            if state.current_block.map(|block| block.kind) != Some(StreamBlockKind::Text) {
                stop_current_stream_block(&mut events, &mut state.current_block);
                let index = next_stream_block_index(&mut state.next_content_block_index);
                events.push(sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "text",
                            "text": ""
                        }
                    }),
                ));
                state.current_block = Some(CurrentBlock {
                    kind: StreamBlockKind::Text,
                    index,
                });
            }
            let index = state.current_block.map(|block| block.index).unwrap_or(0);
            events.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "text_delta",
                        "text": text
                    }
                }),
            ));
            state.output_tokens += text.len() / 4 + 1;
        }
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            if let Some(index_val) = call.get("index").and_then(|v| v.as_u64()) {
                let index = index_val as usize;
                let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let function = call.get("function").cloned().unwrap_or(json!({}));
                let name = function.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = function
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if !name.is_empty() && !state.tool_block_indices.contains_key(&index) {
                    stop_current_stream_block(&mut events, &mut state.current_block);
                    let block_index = next_stream_block_index(&mut state.next_content_block_index);
                    let original_name = tool_name_reverse_map
                        .get(name)
                        .map(|s| s.as_str())
                        .unwrap_or(name);
                    let anthropic_id = openai_id_to_anthropic(call_id);
                    events.push(sse_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": anthropic_id,
                                "name": original_name,
                                "input": {}
                            }
                        }),
                    ));
                    state.tool_block_indices.insert(index, block_index);
                    state.open_tool_blocks.insert(block_index);
                }

                if !arguments.is_empty() {
                    let block_index = state
                        .tool_block_indices
                        .get(&index)
                        .copied()
                        .unwrap_or(index);
                    events.push(sse_event(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": arguments
                            }
                        }),
                    ));
                }
            }
        }
    }

    if finish_reason.is_some() {
        state.ended = true;
        stop_current_stream_block(&mut events, &mut state.current_block);
        close_open_tool_blocks(&mut events, &mut state.open_tool_blocks);
        let stop_reason = match finish_reason {
            Some("stop") => "end_turn",
            Some("length") => "max_tokens",
            Some("content_filter") => "refusal",
            Some("tool_calls") | Some("function_call") => "tool_use",
            _ => "end_turn",
        };
        state.stop_reason_value = Some(stop_reason.to_string());
        state.pending_message_delta = Some(sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": {
                    "output_tokens": state.output_tokens
                }
            }),
        ));
    }

    events
}

pub(crate) async fn handle_stream(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_reverse_map: Arc<HashMap<String, String>>,
    request_id: String,
    request_start: std::time::Instant,
    upstream_start: std::time::Instant,
    upstream_headers_ms: u128,
    log_ctx: Option<StreamLogContext>,
) -> Result<Response> {
    info!(
        request_id = request_id.as_str(),
        upstream_headers_ms, "开始处理上游流式响应"
    );
    let byte_stream = upstream_resp.bytes_stream();
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, AppError>>(128);

    let model = model.to_string();

    tokio::spawn(async move {
        let stream_start = std::time::Instant::now();
        let mut buffer = String::with_capacity(16384);
        let mut utf8_remainder: Vec<u8> = Vec::new();
        let mut conv_state = StreamConversionState::new();
        let mut has_emitted_message_delta = false;
        let mut actual_usage = UsageParts::default();
        let mut upstream_chunks: u64 = 0;
        let mut emitted_events: u64 = 0;

        let log_stream_end = |reason: &str,
                              stream_id: &str,
                              started: bool,
                              ended: bool,
                              upstream_chunks: u64,
                              emitted_events: u64,
                              actual_usage: UsageParts,
                              stop_reason_value: Option<&str>| {
            let (status, error_message) = match reason {
                "done" | "eof_after_finish" | "eof_without_done" => (200, None),
                "client_disconnected" => {
                    (499, Some("stream ended: client disconnected".to_string()))
                }
                "upstream_error" => (502, Some("stream ended: upstream error".to_string())),
                other => (502, Some(format!("stream ended: {}", other))),
            };
            info!(
                request_id = request_id.as_str(),
                stream_id,
                reason,
                started,
                ended,
                upstream_chunks,
                emitted_events,
                actual_input_tokens = actual_usage.input_tokens,
                actual_output_tokens = actual_usage.output_tokens,
                actual_cache_read_input_tokens = actual_usage.cache_read_input_tokens,
                actual_cache_creation_input_tokens = actual_usage.cache_creation_input_tokens,
                stop_reason = stop_reason_value.unwrap_or(""),
                upstream_headers_ms,
                upstream_total_ms = elapsed_ms(upstream_start),
                stream_total_ms = elapsed_ms(stream_start),
                request_total_ms = elapsed_ms(request_start),
                "流式响应结束"
            );
            if let Some(log_ctx) = &log_ctx {
                log_ctx.emit(
                    status,
                    Some(upstream_headers_ms as u64),
                    error_message,
                    None,
                );
            }
        };

        let mut stream = byte_stream;
        // Offset-based buffer to avoid O(n) drain and block-to_string per chunk.
        let mut read_offset: usize = 0;

        while let Some(result) = stream.next().await {
            match result {
                Ok(bytes) => {
                    append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);

                    while let Some(block) = next_sse_block(&buffer, &mut read_offset) {
                        for line in block.lines() {
                            if !line.starts_with("data: ") {
                                continue;
                            }
                            let data = &line[6..];

                            if data == "[DONE]" {
                                upstream_chunks += 1;
                                if actual_usage.has_any() {
                                    let sr = conv_state
                                        .stop_reason_value
                                        .as_deref()
                                        .unwrap_or("end_turn");
                                    let usage = build_anthropic_usage(
                                        actual_usage,
                                        conv_state.output_tokens as u64,
                                    );
                                    let delta = sse_event(
                                        "message_delta",
                                        &json!({
                                            "type": "message_delta",
                                            "delta": {
                                                "stop_reason": sr,
                                                "stop_sequence": null
                                            },
                                            "usage": usage
                                        }),
                                    );
                                    match tx.send(Ok(Bytes::from(delta))).await {
                                        Ok(_) => emitted_events += 1,
                                        Err(_) => {
                                            log_stream_end(
                                                "client_disconnected",
                                                &conv_state.stream_id,
                                                conv_state.started,
                                                conv_state.ended,
                                                upstream_chunks,
                                                emitted_events,
                                                actual_usage,
                                                conv_state.stop_reason_value.as_deref(),
                                            );
                                            return;
                                        }
                                    }
                                } else if let Some(delta) = conv_state.pending_message_delta.take()
                                {
                                    match tx.send(Ok(Bytes::from(delta))).await {
                                        Ok(_) => emitted_events += 1,
                                        Err(_) => {
                                            log_stream_end(
                                                "client_disconnected",
                                                &conv_state.stream_id,
                                                conv_state.started,
                                                conv_state.ended,
                                                upstream_chunks,
                                                emitted_events,
                                                actual_usage,
                                                conv_state.stop_reason_value.as_deref(),
                                            );
                                            return;
                                        }
                                    }
                                }
                                if send_message_stop(&tx).await {
                                    emitted_events += 1;
                                    log_stream_end(
                                        "done",
                                        &conv_state.stream_id,
                                        conv_state.started,
                                        conv_state.ended,
                                        upstream_chunks,
                                        emitted_events,
                                        actual_usage,
                                        conv_state.stop_reason_value.as_deref(),
                                    );
                                } else {
                                    log_stream_end(
                                        "client_disconnected",
                                        &conv_state.stream_id,
                                        conv_state.started,
                                        conv_state.ended,
                                        upstream_chunks,
                                        emitted_events,
                                        actual_usage,
                                        conv_state.stop_reason_value.as_deref(),
                                    );
                                }
                                return;
                            }

                            if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                                upstream_chunks += 1;
                                if let Some(usage) = chunk.get("usage") {
                                    actual_usage.merge(extract_openai_usage_parts(usage));
                                    if chunk
                                        .get("choices")
                                        .and_then(|c| c.as_array())
                                        .map(|arr| arr.is_empty())
                                        .unwrap_or(false)
                                    {
                                        continue;
                                    }
                                }

                                let events = convert_stream_chunk(
                                    &chunk,
                                    &model,
                                    &mut conv_state,
                                    &tool_name_reverse_map,
                                );

                                for event in events {
                                    match tx.send(Ok(Bytes::from(event))).await {
                                        Ok(_) => emitted_events += 1,
                                        Err(_) => {
                                            log_stream_end(
                                                "client_disconnected",
                                                &conv_state.stream_id,
                                                conv_state.started,
                                                conv_state.ended,
                                                upstream_chunks,
                                                emitted_events,
                                                actual_usage,
                                                conv_state.stop_reason_value.as_deref(),
                                            );
                                            return;
                                        }
                                    }
                                }

                                if conv_state.ended {
                                    has_emitted_message_delta = true;
                                }
                            }
                        }

                        compact_sse_buffer(&mut buffer, &mut read_offset);
                    }
                }
                Err(e) => {
                    error!(
                        request_id = request_id.as_str(),
                        error = %e,
                        upstream_headers_ms,
                        upstream_total_ms = elapsed_ms(upstream_start),
                        stream_total_ms = elapsed_ms(stream_start),
                        request_total_ms = elapsed_ms(request_start),
                        "上游流式读取错误"
                    );
                    let _ = tx.send(Err(AppError::Http(e))).await;
                    log_stream_end(
                        "upstream_error",
                        &conv_state.stream_id,
                        conv_state.started,
                        conv_state.ended,
                        upstream_chunks,
                        emitted_events,
                        actual_usage,
                        conv_state.stop_reason_value.as_deref(),
                    );
                    return;
                }
            }
        }

        if conv_state.started && !conv_state.ended {
            emitted_events +=
                send_final_events(&tx, &conv_state.current_block, &conv_state.open_tool_blocks)
                    .await;
            if send_message_stop(&tx).await {
                emitted_events += 1;
            }
            log_stream_end(
                "eof_without_done",
                &conv_state.stream_id,
                conv_state.started,
                conv_state.ended,
                upstream_chunks,
                emitted_events,
                actual_usage,
                conv_state.stop_reason_value.as_deref(),
            );
        } else if conv_state.started && conv_state.ended {
            if !has_emitted_message_delta {
                if let Some(delta) = conv_state.pending_message_delta.take() {
                    if tx.send(Ok(Bytes::from(delta))).await.is_ok() {
                        emitted_events += 1;
                    }
                }
                if send_message_stop(&tx).await {
                    emitted_events += 1;
                }
            }
            log_stream_end(
                "eof_after_finish",
                &conv_state.stream_id,
                conv_state.started,
                conv_state.ended,
                upstream_chunks,
                emitted_events,
                actual_usage,
                conv_state.stop_reason_value.as_deref(),
            );
        } else {
            log_stream_end(
                "eof_before_start",
                &conv_state.stream_id,
                conv_state.started,
                conv_state.ended,
                upstream_chunks,
                emitted_events,
                actual_usage,
                conv_state.stop_reason_value.as_deref(),
            );
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

// ---- Anthropic SSE → OpenAI SSE conversion ----

/// State for converting Anthropic streaming responses to OpenAI SSE format.
pub(crate) struct OpenAiStreamOutputState {
    pub(crate) stream_id: String,
    pub(crate) started: bool,
    pub(crate) current_block_type: Option<String>,
    pub(crate) tool_call_counter: usize,
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
                    let index = state.tool_call_counter;
                    state.tool_call_counter += 1;

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
                            // Use the last tool call index (most recently started)
                            let index = state.tool_call_counter.saturating_sub(1);
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

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Parse an Anthropic SSE block to extract event type and JSON data.
fn parse_anthropic_sse_block(block: &str) -> Option<(String, Value)> {
    let mut event_type = String::new();
    let mut data_str = String::new();

    for line in block.lines() {
        if let Some(et) = line.strip_prefix("event: ") {
            event_type = et.trim().to_string();
        } else if let Some(d) = line.strip_prefix("data: ") {
            data_str = d.trim().to_string();
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
                    log_ctx.emit(status, Some(upstream_headers_ms as u64), error_message, None);
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

#[cfg(test)]
mod log_tests {
    use super::StreamLogContext;
    use crate::logging::{LogCollector, LogConfig};
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn stream_log_context_uses_completion_time_for_duration() {
        let log_config = Arc::new(ArcSwap::from_pointee(LogConfig::default()));
        let collector = Arc::new(LogCollector::new(log_config, 8));
        let mut receiver = collector.sender.subscribe();
        let now = Instant::now();
        let request_start = now - Duration::from_millis(120);
        let upstream_start = now - Duration::from_millis(100);

        let ctx = StreamLogContext {
            collector,
            request_id: "req_test".to_string(),
            method: "POST",
            path: "/v1/messages",
            provider: "provider".to_string(),
            model: "model".to_string(),
            requested_model: "requested".to_string(),
            request_start,
            upstream_start,
            raw_request_body: "{}".to_string(),
        };

        ctx.emit(200, Some(30), None, None);
        let entry = receiver.try_recv().unwrap();

        assert!(entry.duration_ms >= 120);
        assert_eq!(entry.proxy_overhead_ms, Some(20));
        assert_eq!(entry.ttft_ms, Some(30));
        assert!(entry.is_stream);
    }
}
