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
use tokio::sync::mpsc;
use tracing::{error, info};

use super::convert::openai_id_to_anthropic;
use super::state::elapsed_ms;
use super::utils::{append_utf8_safe, find_sse_block_end};
use crate::error::{AppError, Result};

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
    events.push(sse_event(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    ));
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
            .send(Ok(Bytes::from(sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": block.index}),
            ))))
            .await
            .is_ok()
        {
            sent += 1;
        }
    }

    for index in open_tool_blocks {
        if tx
            .send(Ok(Bytes::from(sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            ))))
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
    stream_id: &mut String,
    started: &mut bool,
    current_block: &mut Option<CurrentBlock>,
    next_content_block_index: &mut usize,
    ended: &mut bool,
    pending_message_delta: &mut Option<String>,
    output_tokens: &mut usize,
    tool_name_reverse_map: &HashMap<String, String>,
    tool_block_indices: &mut HashMap<usize, usize>,
    open_tool_blocks: &mut BTreeSet<usize>,
    stop_reason_value: &mut Option<String>,
) -> StreamEvents {
    let mut events = StreamEvents::new();

    let id = chunk
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if !id.is_empty() && stream_id.is_empty() {
        *stream_id = id;
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

    if !*started && delta.get("role").is_some() {
        *started = true;
        events.push(sse_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": stream_id,
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
            close_open_tool_blocks(&mut events, open_tool_blocks);
            if current_block.map(|block| block.kind) != Some(StreamBlockKind::Thinking) {
                stop_current_stream_block(&mut events, current_block);
                let index = next_stream_block_index(next_content_block_index);
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
                *current_block = Some(CurrentBlock {
                    kind: StreamBlockKind::Thinking,
                    index,
                });
            }
            let index = current_block.map(|block| block.index).unwrap_or(0);
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
            *output_tokens += text.len() / 4 + 1;
        }
    }

    if let Some(text) = content {
        if !text.is_empty() {
            close_open_tool_blocks(&mut events, open_tool_blocks);
            if current_block.map(|block| block.kind) != Some(StreamBlockKind::Text) {
                stop_current_stream_block(&mut events, current_block);
                let index = next_stream_block_index(next_content_block_index);
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
                *current_block = Some(CurrentBlock {
                    kind: StreamBlockKind::Text,
                    index,
                });
            }
            let index = current_block.map(|block| block.index).unwrap_or(0);
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
            *output_tokens += text.len() / 4 + 1;
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

                if !name.is_empty() && !tool_block_indices.contains_key(&index) {
                    stop_current_stream_block(&mut events, current_block);
                    let block_index = next_stream_block_index(next_content_block_index);
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
                    tool_block_indices.insert(index, block_index);
                    open_tool_blocks.insert(block_index);
                }

                if !arguments.is_empty() {
                    let block_index = tool_block_indices.get(&index).copied().unwrap_or(index);
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
        *ended = true;
        stop_current_stream_block(&mut events, current_block);
        close_open_tool_blocks(&mut events, open_tool_blocks);
        let stop_reason = match finish_reason {
            Some("stop") => "end_turn",
            Some("length") => "max_tokens",
            Some("content_filter") => "end_turn",
            Some("tool_calls") | Some("function_call") => "tool_use",
            _ => "end_turn",
        };
        *stop_reason_value = Some(stop_reason.to_string());
        *pending_message_delta = Some(sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": {
                    "output_tokens": *output_tokens
                }
            }),
        ));
    }

    events
}

pub(crate) async fn handle_stream(
    upstream_resp: reqwest::Response,
    model: &str,
    _estimated_input_tokens: u64,
    tool_name_reverse_map: Arc<HashMap<String, String>>,
    request_id: String,
    request_start: std::time::Instant,
    upstream_start: std::time::Instant,
    upstream_headers_ms: u128,
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
        let mut stream_id = String::new();
        let mut started = false;
        let mut ended = false;
        let mut current_block: Option<CurrentBlock> = None;
        let mut next_content_block_index: usize = 0;
        let mut output_tokens: usize = 0;
        let mut pending_message_delta: Option<String> = None;
        let mut has_emitted_message_delta = false;
        let mut tool_block_indices: HashMap<usize, usize> = HashMap::new();
        let mut open_tool_blocks: BTreeSet<usize> = BTreeSet::new();
        let mut actual_usage = UsageParts::default();
        let mut stop_reason_value: Option<String> = None;
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
        };

        let mut stream = byte_stream;
        // Offset-based buffer to avoid O(n) drain and block-to_string per chunk.
        let mut read_offset: usize = 0;

        while let Some(result) = stream.next().await {
            match result {
                Ok(bytes) => {
                    append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);

                    loop {
                        if let Some(block) = next_sse_block(&buffer, &mut read_offset) {
                            for line in block.lines() {
                                if !line.starts_with("data: ") {
                                    continue;
                                }
                                let data = &line[6..];

                                if data == "[DONE]" {
                                    upstream_chunks += 1;
                                    if actual_usage.has_any() {
                                        let sr = stop_reason_value.as_deref().unwrap_or("end_turn");
                                        let usage = build_anthropic_usage(
                                            actual_usage,
                                            output_tokens as u64,
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
                                                    &stream_id,
                                                    started,
                                                    ended,
                                                    upstream_chunks,
                                                    emitted_events,
                                                    actual_usage,
                                                    stop_reason_value.as_deref(),
                                                );
                                                return;
                                            }
                                        }
                                    } else if let Some(delta) = pending_message_delta.take() {
                                        match tx.send(Ok(Bytes::from(delta))).await {
                                            Ok(_) => emitted_events += 1,
                                            Err(_) => {
                                                log_stream_end(
                                                    "client_disconnected",
                                                    &stream_id,
                                                    started,
                                                    ended,
                                                    upstream_chunks,
                                                    emitted_events,
                                                    actual_usage,
                                                    stop_reason_value.as_deref(),
                                                );
                                                return;
                                            }
                                        }
                                    }
                                    if send_message_stop(&tx).await {
                                        emitted_events += 1;
                                        log_stream_end(
                                            "done",
                                            &stream_id,
                                            started,
                                            ended,
                                            upstream_chunks,
                                            emitted_events,
                                            actual_usage,
                                            stop_reason_value.as_deref(),
                                        );
                                    } else {
                                        log_stream_end(
                                            "client_disconnected",
                                            &stream_id,
                                            started,
                                            ended,
                                            upstream_chunks,
                                            emitted_events,
                                            actual_usage,
                                            stop_reason_value.as_deref(),
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
                                        &mut stream_id,
                                        &mut started,
                                        &mut current_block,
                                        &mut next_content_block_index,
                                        &mut ended,
                                        &mut pending_message_delta,
                                        &mut output_tokens,
                                        &tool_name_reverse_map,
                                        &mut tool_block_indices,
                                        &mut open_tool_blocks,
                                        &mut stop_reason_value,
                                    );

                                    for event in events {
                                        match tx.send(Ok(Bytes::from(event))).await {
                                            Ok(_) => emitted_events += 1,
                                            Err(_) => {
                                                log_stream_end(
                                                    "client_disconnected",
                                                    &stream_id,
                                                    started,
                                                    ended,
                                                    upstream_chunks,
                                                    emitted_events,
                                                    actual_usage,
                                                    stop_reason_value.as_deref(),
                                                );
                                                return;
                                            }
                                        }
                                    }

                                    if ended {
                                        has_emitted_message_delta = true;
                                    }
                                }
                            }

                            compact_sse_buffer(&mut buffer, &mut read_offset);
                        } else {
                            break;
                        }
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
                        &stream_id,
                        started,
                        ended,
                        upstream_chunks,
                        emitted_events,
                        actual_usage,
                        stop_reason_value.as_deref(),
                    );
                    return;
                }
            }
        }

        if started && !ended {
            emitted_events += send_final_events(&tx, &current_block, &open_tool_blocks).await;
            if send_message_stop(&tx).await {
                emitted_events += 1;
            }
            log_stream_end(
                "eof_without_done",
                &stream_id,
                started,
                ended,
                upstream_chunks,
                emitted_events,
                actual_usage,
                stop_reason_value.as_deref(),
            );
        } else if started && ended {
            if !has_emitted_message_delta {
                if let Some(delta) = pending_message_delta.take() {
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
                &stream_id,
                started,
                ended,
                upstream_chunks,
                emitted_events,
                actual_usage,
                stop_reason_value.as_deref(),
            );
        } else {
            log_stream_end(
                "eof_before_start",
                &stream_id,
                started,
                ended,
                upstream_chunks,
                emitted_events,
                actual_usage,
                stop_reason_value.as_deref(),
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

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body_stream))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}
