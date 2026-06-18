//! OpenAI Chat Completions SSE → Anthropic Messages SSE streaming conversion.

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::common::{
    build_anthropic_usage, close_open_tool_blocks, compact_sse_buffer, extract_openai_usage_parts,
    next_sse_block, next_stream_block_index, sse_content_block_stop, sse_event, sse_event_literal,
    stop_current_stream_block, CurrentBlock, StreamBlockKind, StreamEvents, UsageParts,
};
use super::log_context::StreamLogContext;
use super::super::request::openai_id_to_anthropic;
use crate::convert::utils::append_utf8_safe;
use crate::error::{AppError, Result};
use crate::server::state::elapsed_ms;

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

    if !state.started && (delta.get("role").is_some() || content.is_some() || reasoning.is_some() || delta.get("tool_calls").is_some()) {
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
                        .unwrap_or_else(|| {
                            warn!(tool_index = index, "Tool delta fallback: no block index mapping for tool index");
                            index
                        });
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
            if actual_usage.has_any() {
                let sr = conv_state
                    .stop_reason_value
                    .as_deref()
                    .unwrap_or("end_turn");
                let usage = build_anthropic_usage(actual_usage, conv_state.output_tokens as u64);
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
                if tx.send(Ok(Bytes::from(delta))).await.is_ok() {
                    emitted_events += 1;
                }
            } else if let Some(delta) = conv_state.pending_message_delta.take() {
                if tx.send(Ok(Bytes::from(delta))).await.is_ok() {
                    emitted_events += 1;
                }
            }
            if send_message_stop(&tx).await {
                emitted_events += 1;
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
