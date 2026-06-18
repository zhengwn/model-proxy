//! Kiro EventStream → Anthropic SSE streaming handlers (direct and buffered).

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::state::{
    process_event, sse_content_block_stop, sse_event, sse_event_literal, AnthropicStreamState,
    MAX_EVENT_BUFFER_BYTES,
};
use super::KEEP_ALIVE_BYTES;
use crate::convert::anthropic_openai::stream::StreamLogContext;
use crate::convert::kiro::eventstream::{Event, EventStreamDecoder};
use crate::convert::kiro::thinking_parser::{ThinkingHandlingMode, ThinkingOutput, ThinkingParser};
use crate::convert::kiro::truncation::{check_tool_call_truncation, TruncationReason, TruncationState};
use crate::error::{AppError, Result};
use crate::server::state::elapsed_ms;

/// Handle a Kiro EventStream response, converting to Anthropic SSE format.
///
/// If `initial_bytes` is provided, it contains the first chunk already read
/// (from first-token timeout retry), and the `byte_stream` starts from the
/// second chunk onwards.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_stream_anthropic_output(
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
    truncation_state: Option<TruncationState>,
    initial_bytes: Option<Bytes>,
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
        let mut state = AnthropicStreamState::new();
        let mut truncation_detected = false;
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
        // If initial_bytes were provided (first-token retry succeeded), process them
        // before entering the main loop and skip first-token timeout check.
        let mut first_chunk = initial_bytes.is_none();
        if let Some(bytes) = initial_bytes {
            last_data_sent.store(stream_base.elapsed().as_secs(), Ordering::Relaxed);
            if let Err(e) = decoder.feed(&bytes) {
                error!(error = %e, "EventStream feed 错误 (initial_bytes)");
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
                                let sse_events = process_event(&event, &model, &mut state, &tool_name_map);
                                for evt in sse_events {
                                    match tx.send(Ok(Bytes::from(evt))).await {
                                        Ok(_) => emitted_events += 1,
                                        Err(_) => {
                                            log_stream_end("client_disconnected", &state, upstream_chunks, emitted_events);
                                            return;
                                        }
                                    }
                                    if state.ended { has_emitted_message_delta = true; }
                                }
                            }
                            Err(e) => warn!(error = %e, "Event 解析错误 (initial_bytes)，跳过"),
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if decoder.is_stopped() {
                            error!(error = %e, "EventStream 解码器已停止 (initial_bytes)");
                            let _ = tx.send(Err(AppError::Request(e.to_string()))).await;
                            log_stream_end("upstream_error", &state, upstream_chunks, emitted_events);
                            return;
                        }
                        break;
                    }
                }
            }
        }

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
                            // Force-close on ContentLengthExceededException: exit read loop for immediate finalization
                            if state.force_close {
                                break;
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
                    // Send error and close events so client doesn't hang
                    if state.started {
                        let mut timeout_events = Vec::new();
                        state.stop_current_block(&mut timeout_events);
                        state.close_open_tool_blocks(&mut timeout_events);
                        for evt in timeout_events {
                            let _ = tx.send(Ok(Bytes::from(evt))).await;
                        }
                        state.ensure_min_output_tokens();
                        let stop_reason = if first_chunk { "error" } else { state.get_stop_reason() };
                        let delta = sse_event("message_delta", &json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                            "usage": {"output_tokens": state.output_tokens}
                        }));
                        let _ = tx.send(Ok(Bytes::from(delta))).await;
                        let msg_stop = sse_event_literal("message_stop", r#"{"type":"message_stop"}"#);
                        let _ = tx.send(Ok(Bytes::from(msg_stop))).await;
                    }
                    log_stream_end(reason, &state, upstream_chunks, emitted_events);
                    return;
                }
            }
        }

        // Stream ended — emit final events
        if state.started {
            // Close any open blocks — send stop events to client
            let mut block_stop_events = Vec::new();
            state.stop_current_block(&mut block_stop_events);
            state.close_open_tool_blocks(&mut block_stop_events);
            for evt in block_stop_events {
                let _ = tx.send(Ok(Bytes::from(evt))).await;
                emitted_events += 1;
            }

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
                                let mut stop_events = Vec::new();
                                state.stop_current_block(&mut stop_events);
                                for evt in stop_events {
                                    let _ = tx.send(Ok(Bytes::from(evt))).await;
                                    emitted_events += 1;
                                }
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

            // Detect and store truncation info for recovery on the next request
            if let Some(ref ts) = truncation_state {
                // Check for truncated tool calls
                for (tool_use_id, buffer) in &state.tool_input_buffers {
                    if let Some(reason) = check_tool_call_truncation(tool_use_id, buffer) {
                        ts.store_tool_truncation(tool_use_id.clone(), reason).await;
                        truncation_detected = true;
                    }
                }
                // Check for content truncation (missing usage event)
                if state.input_tokens == 0 && state.output_tokens > 0 {
                    ts.store_content_truncation(TruncationReason::MissingUsage).await;
                    truncation_detected = true;
                }
                if truncation_detected {
                    warn!(
                        request_id = request_id.as_str(),
                        "检测到流截断，已存储截断信息用于下次请求恢复"
                    );
                }
            }

            // Emit message_delta with stop_reason and usage
            if !has_emitted_message_delta {
                state.ensure_min_output_tokens();
                let stop_reason = state.get_stop_reason().to_string();
                let delta = sse_event(
                    "message_delta",
                    &json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                        "usage": {"input_tokens": state.input_tokens, "output_tokens": state.output_tokens}
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

/// Buffered variant for `/cc/v1/messages` — buffers all SSE events until the
/// upstream stream ends, then patches `message_start` with accurate `input_tokens`
/// from `ContextUsage` before flushing everything to the client.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_stream_anthropic_output_buffered(
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
    truncation_state: Option<TruncationState>,
    initial_bytes: Option<Bytes>,
) -> Result<Response> {
    info!(
        request_id = request_id.as_str(),
        upstream_headers_ms, "开始处理 Kiro 流式响应（缓冲模式），转换为 Anthropic 格式"
    );

    let byte_stream = upstream_resp.bytes_stream();
    let model = model.to_string();
    let thinking_mode_owned: Option<String> = thinking_mode.map(|s| s.to_string());

    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, AppError>>(128);
    let keepalive_tx = tx.clone();
    let stream_base = Instant::now();
    let last_data_sent = Arc::new(AtomicU64::new(0));

    // Spawn keep-alive task (sends pings during buffering)
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
        let mut truncation_detected = false;
        if let Some(ref mode_str) = thinking_mode_owned {
            let mode = ThinkingHandlingMode::from_str(mode_str);
            state.thinking_parser = Some(ThinkingParser::new(mode));
        }
        let mut upstream_chunks: u64 = 0;
        let mut event_buffer: Vec<String> = Vec::new();
        let mut event_buffer_bytes: usize = 0;

        let log_stream_end = |reason: &str, state: &AnthropicStreamState, upstream_chunks: u64, buffered: u64| {
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
                upstream_chunks,
                buffered_events = buffered,
                input_tokens = state.input_tokens,
                output_tokens = state.completion_tokens,
                stop_reason = state.get_stop_reason(),
                upstream_headers_ms,
                upstream_total_ms = elapsed_ms(upstream_start),
                stream_total_ms = elapsed_ms(stream_start),
                request_total_ms = elapsed_ms(request_start),
                "Kiro→Anthropic 缓冲流式响应结束"
            );
            if let Some(log_ctx) = &log_ctx {
                log_ctx.emit(status, Some(upstream_headers_ms as u64), error_message, None);
            }
        };

        let mut stream = byte_stream;
        // If initial_bytes were provided (first-token retry succeeded), process them
        // before entering the main loop and skip first-token timeout check.
        let mut first_chunk = initial_bytes.is_none();
        if let Some(bytes) = initial_bytes {
            last_data_sent.store(stream_base.elapsed().as_secs(), Ordering::Relaxed);
            if let Err(e) = decoder.feed(&bytes) {
                error!(error = %e, "EventStream feed 错误 (initial_bytes)");
                let _ = tx.send(Err(AppError::Request(e.to_string()))).await;
                log_stream_end("upstream_error", &state, upstream_chunks, event_buffer.len() as u64);
                return;
            }
            loop {
                match decoder.decode() {
                    Ok(Some(frame)) => {
                        upstream_chunks += 1;
                        match Event::from_frame(&frame) {
                            Ok(event) => {
                                let sse_events = process_event(&event, &model, &mut state, &tool_name_map);
                                for evt in &sse_events {
                                    event_buffer_bytes += evt.len();
                                }
                                event_buffer.extend(sse_events);
                                if event_buffer_bytes > MAX_EVENT_BUFFER_BYTES {
                                    warn!(
                                        event_buffer_bytes,
                                        limit = MAX_EVENT_BUFFER_BYTES,
                                        "缓冲事件大小超限，强制结束流"
                                    );
                                    state.force_close = true;
                                    break;
                                }
                            }
                            Err(e) => warn!(error = %e, "Event 解析错误 (initial_bytes)，跳过"),
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if decoder.is_stopped() {
                            error!(error = %e, "EventStream 解码器已停止 (initial_bytes)");
                            let _ = tx.send(Err(AppError::Request(e.to_string()))).await;
                            log_stream_end("upstream_error", &state, upstream_chunks, event_buffer.len() as u64);
                            return;
                        }
                        break;
                    }
                }
            }
        }

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
                            last_data_sent.store(stream_base.elapsed().as_secs(), Ordering::Relaxed);
                            first_chunk = false;

                            if let Err(e) = decoder.feed(&bytes) {
                                error!(error = %e, "EventStream feed 错误");
                                let _ = tx.send(Err(AppError::Request(e.to_string()))).await;
                                log_stream_end("upstream_error", &state, upstream_chunks, event_buffer.len() as u64);
                                return;
                            }

                            loop {
                                match decoder.decode() {
                                    Ok(Some(frame)) => {
                                        upstream_chunks += 1;
                                        match Event::from_frame(&frame) {
                                            Ok(event) => {
                                                let sse_events = process_event(
                                                    &event, &model, &mut state, &tool_name_map,
                                                );
                                                // Buffer events instead of sending immediately
                                                for evt in &sse_events {
                                                    event_buffer_bytes += evt.len();
                                                }
                                                event_buffer.extend(sse_events);
                                                if event_buffer_bytes > MAX_EVENT_BUFFER_BYTES {
                                                    warn!(
                                                        event_buffer_bytes,
                                                        limit = MAX_EVENT_BUFFER_BYTES,
                                                        "缓冲事件大小超限，强制结束流"
                                                    );
                                                    state.force_close = true;
                                                    break;
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
                                            log_stream_end("upstream_error", &state, upstream_chunks, event_buffer.len() as u64);
                                            return;
                                        }
                                        break;
                                    }
                                }
                            }
                            // Force-close on ContentLengthExceededException
                            if state.force_close {
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            error!(request_id = request_id.as_str(), error = %e, "Kiro 流式读取错误");
                            let _ = tx.send(Err(AppError::Http(e))).await;
                            log_stream_end("upstream_error", &state, upstream_chunks, event_buffer.len() as u64);
                            return;
                        }
                        None => break,
                    }
                }
                _ = &mut keepalive_timer => {
                    let reason = if first_chunk { "first_token_timeout" } else { "stream_timeout" };
                    warn!("Kiro 流式超时: {}", reason);
                    // Send close events so client doesn't hang
                    if state.started {
                        let mut timeout_events = Vec::new();
                        state.stop_current_block(&mut timeout_events);
                        state.close_open_tool_blocks(&mut timeout_events);
                        for evt in timeout_events {
                            let _ = tx.send(Ok(Bytes::from(evt))).await;
                        }
                        state.ensure_min_output_tokens();
                        let stop_reason = if first_chunk { "error" } else { state.get_stop_reason() };
                        let delta = sse_event("message_delta", &json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                            "usage": {"output_tokens": state.output_tokens}
                        }));
                        let _ = tx.send(Ok(Bytes::from(delta))).await;
                        let msg_stop = sse_event_literal("message_stop", r#"{"type":"message_stop"}"#);
                        let _ = tx.send(Ok(Bytes::from(msg_stop))).await;
                    }
                    log_stream_end(reason, &state, upstream_chunks, event_buffer.len() as u64);
                    return;
                }
            }
        }

        // Stream ended — flush buffered events with corrected input_tokens
        if state.started {
            // Close any open blocks into buffer
            state.stop_current_block(&mut event_buffer);
            state.close_open_tool_blocks(&mut event_buffer);

            // Finalize ThinkingParser
            if let Some(ref mut parser) = state.thinking_parser {
                let final_outputs = parser.finalize();
                for output in final_outputs {
                    match output {
                        ThinkingOutput::ThinkingDelta(text) => {
                            if !state.in_thinking {
                                let idx = state.alloc_block_index();
                                event_buffer.push(sse_event("content_block_start", &json!({
                                    "type": "content_block_start", "index": idx,
                                    "content_block": {"type": "thinking", "thinking": ""}
                                })));
                                state.in_thinking = true;
                                state.current_block_type = Some("thinking".to_string());
                                state.current_block_index = Some(idx);
                            }
                            let idx = state.current_block_index.unwrap_or(0);
                            event_buffer.push(sse_event("content_block_delta", &json!({
                                "type": "content_block_delta", "index": idx,
                                "delta": {"type": "thinking_delta", "thinking": text}
                            })));
                        }
                        ThinkingOutput::ContentDelta(text) => {
                            if state.in_thinking {
                                state.stop_current_block(&mut event_buffer);
                            }
                            let idx = state.alloc_block_index();
                            event_buffer.push(sse_event("content_block_start", &json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": {"type": "text", "text": ""}
                            })));
                            event_buffer.push(sse_event("content_block_delta", &json!({
                                "type": "content_block_delta", "index": idx,
                                "delta": {"type": "text_delta", "text": text}
                            })));
                            event_buffer.push(sse_content_block_stop(idx));
                        }
                        ThinkingOutput::None => {}
                    }
                }
            }

            // Detect and store truncation info for recovery on the next request
            if let Some(ref ts) = truncation_state {
                for (tool_use_id, buffer) in &state.tool_input_buffers {
                    if let Some(reason) = check_tool_call_truncation(tool_use_id, buffer) {
                        ts.store_tool_truncation(tool_use_id.clone(), reason).await;
                        truncation_detected = true;
                    }
                }
                if state.input_tokens == 0 && state.output_tokens > 0 {
                    ts.store_content_truncation(TruncationReason::MissingUsage).await;
                    truncation_detected = true;
                }
                if truncation_detected {
                    warn!(
                        request_id = request_id.as_str(),
                        "检测到流截断（缓冲模式），已存储截断信息用于下次请求恢复"
                    );
                }
            }

            // Patch message_start with accurate input_tokens
            let final_input_tokens = state.input_tokens;
            for evt_str in &mut event_buffer {
                if evt_str.contains("\"message_start\"") && evt_str.contains("\"input_tokens\"") {
                    // Parse, patch, and re-serialize
                    if let Ok(mut evt_json) = serde_json::from_str::<serde_json::Value>(
                        evt_str.lines().find(|l| l.starts_with("data: ")).map(|l| &l[6..]).unwrap_or("")
                    ) {
                        if let Some(usage) = evt_json.pointer_mut("/message/usage") {
                            usage["input_tokens"] = json!(final_input_tokens);
                        }
                        let patched = serde_json::to_string(&evt_json).unwrap_or_default();
                        *evt_str = format!("event: message_start\ndata: {}\n\n", patched);
                    }
                    break;
                }
            }

            // Flush all buffered events
            let mut emitted_events: u64 = 0;
            for evt in event_buffer {
                if tx.send(Ok(Bytes::from(evt))).await.is_err() {
                    log_stream_end("client_disconnected", &state, upstream_chunks, emitted_events);
                    return;
                }
                emitted_events += 1;
            }

            // Emit message_delta
            state.ensure_min_output_tokens();
            let stop_reason = state.get_stop_reason().to_string();
            let delta = sse_event("message_delta", &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {"input_tokens": final_input_tokens, "output_tokens": state.output_tokens}
            }));
            let _ = tx.send(Ok(Bytes::from(delta))).await;
            emitted_events += 1;

            // Emit message_stop
            let msg_stop = sse_event_literal("message_stop", r#"{"type":"message_stop"}"#);
            let _ = tx.send(Ok(Bytes::from(msg_stop))).await;
            emitted_events += 1;

            log_stream_end("done", &state, upstream_chunks, emitted_events);
        } else {
            log_stream_end("eof", &state, upstream_chunks, 0);
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
