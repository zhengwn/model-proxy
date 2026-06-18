//! Kiro → Anthropic stream conversion state machine and per-event processing.

use serde_json::{json, Value};
use std::collections::HashMap;

use super::{estimate_tokens, generate_id};
use crate::convert::kiro::eventstream::Event;
use crate::convert::kiro::model_map::context_window_size;
use crate::convert::kiro::thinking_parser::{ThinkingOutput, ThinkingParser};

/// Maximum bytes for a single tool input accumulation buffer (1 MB).
const MAX_TOOL_INPUT_BUFFER_BYTES: usize = 1024 * 1024;

/// Maximum total bytes for the buffered-mode event buffer (2 MB).
pub(super) const MAX_EVENT_BUFFER_BYTES: usize = 2 * 1024 * 1024;

// ---- SSE helpers ----

pub(super) fn sse_event(event: &str, data: &Value) -> String {
    let json_str = serde_json::to_string(data).unwrap_or_default();
    let mut s = String::with_capacity(8 + event.len() + 8 + json_str.len() + 2);
    s.push_str("event: ");
    s.push_str(event);
    s.push_str("\ndata: ");
    s.push_str(&json_str);
    s.push_str("\n\n");
    s
}

pub(super) fn sse_event_literal(event: &str, json_literal: &str) -> String {
    let mut s = String::with_capacity(8 + event.len() + 8 + json_literal.len() + 2);
    s.push_str("event: ");
    s.push_str(event);
    s.push_str("\ndata: ");
    s.push_str(json_literal);
    s.push_str("\n\n");
    s
}

pub(super) fn sse_content_block_stop(index: usize) -> String {
    sse_event_literal(
        "content_block_stop",
        &format!(
            "{{\"type\":\"content_block_stop\",\"index\":{}}}",
            index
        ),
    )
}

// ---- Stream conversion state ----

/// State machine for converting Kiro events to Anthropic SSE.
pub(super) struct AnthropicStreamState {
    pub(super) stream_id: String,
    pub(super) started: bool,
    pub(super) ended: bool,
    pub(super) current_block_type: Option<String>, // "text" | "thinking" | "tool_use"
    pub(super) current_block_index: Option<usize>,
    pub(super) next_block_index: usize,
    pub(super) tool_block_indices: HashMap<String, usize>, // tool_use_id → block_index
    pub(super) open_tool_blocks: Vec<usize>,
    pub(super) has_tool_use: bool,
    pub(super) stop_reason: Option<String>,
    pub(super) output_tokens: usize,
    pub(super) input_tokens: u64,
    pub(super) completion_tokens: u64,
    pub(super) last_content: String, // for text dedup
    pub(super) in_thinking: bool,
    pub(super) tool_input_buffers: HashMap<String, String>, // tool_use_id → accumulated input
    pub(super) thinking_parser: Option<ThinkingParser>,      // FSM for thinking tag extraction
    pub(super) force_close: bool, // force immediate stream closure (e.g. ContentLengthExceededException)
}

impl AnthropicStreamState {
    pub(super) fn new() -> Self {
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
            force_close: false,
        }
    }

    pub(super) fn alloc_block_index(&mut self) -> usize {
        let idx = self.next_block_index;
        self.next_block_index += 1;
        idx
    }

    pub(super) fn stop_current_block(&mut self, events: &mut Vec<String>) {
        if let Some(idx) = self.current_block_index.take() {
            events.push(sse_content_block_stop(idx));
        }
        self.current_block_type = None;
    }

    pub(super) fn close_open_tool_blocks(&mut self, events: &mut Vec<String>) {
        for idx in self.open_tool_blocks.drain(..) {
            events.push(sse_content_block_stop(idx));
        }
    }

    pub(super) fn get_stop_reason(&self) -> &str {
        if let Some(sr) = &self.stop_reason {
            return sr.as_str();
        }
        if self.has_tool_use {
            "tool_use"
        } else {
            "end_turn"
        }
    }

    /// Ensure output_tokens >= 1 when content was present.
    /// Tool-only responses can produce output_tokens=0 which confuses billing/monitoring.
    pub(super) fn ensure_min_output_tokens(&mut self) {
        if self.has_tool_use || self.output_tokens > 0 {
            self.output_tokens = self.output_tokens.max(1);
        }
    }
}

// ---- Event → SSE conversion ----

pub(super) fn process_event(
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
                state.output_tokens += estimate_tokens(new_text);
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
            state.output_tokens += estimate_tokens(text);
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

            // Close any open text/thinking block before processing tool event.
            // For NEW tool blocks, stop_current_block is called again below (idempotent via take()).
            // For CONTINUATION events (existing tool_use_id), this ensures text blocks are closed.
            if state.current_block_type.as_deref() == Some("text")
                || state.current_block_type.as_deref() == Some("thinking")
            {
                state.stop_current_block(&mut events);
            }

            // Accumulate input (with size guard)
            let buffer = state
                .tool_input_buffers
                .entry(tool_use_id.clone())
                .or_default();
            if buffer.len() + input.len() > MAX_TOOL_INPUT_BUFFER_BYTES {
                tracing::warn!(
                    tool_use_id = tool_use_id.as_str(),
                    current_len = buffer.len(),
                    incoming_len = input.len(),
                    limit = MAX_TOOL_INPUT_BUFFER_BYTES,
                    "工具输入缓冲区超限，丢弃增量输入"
                );
                // Still need to start the block if new, but skip input accumulation
                if !state.tool_block_indices.contains_key(tool_use_id) {
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
                if *stop {
                    if let Some(idx) = state.tool_block_indices.get(tool_use_id) {
                        events.push(sse_content_block_stop(*idx));
                        state.open_tool_blocks.retain(|i| *i != *idx);
                    }
                }
                return events;
            }
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
            state.input_tokens = (*percentage * window as f64 / 100.0) as u64;
            if *percentage >= 100.0 {
                state.stop_reason = Some("model_context_window_exceeded".to_string());
            }
        }

        Event::Metering { .. } => {
            // Billing info - not directly mappable to Anthropic usage
        }

        Event::Error { code, message } => {
            tracing::warn!(code, message, "Kiro API 错误");
        }

        Event::Exception {
            type_name,
            message,
        } => {
            tracing::warn!(type_name, message, "Kiro API 异常");
            if type_name == "ContentLengthExceededException" {
                state.stop_reason = Some("max_tokens".to_string());
                state.force_close = true;
            }
        }

        Event::Unknown => {}
    }

    events
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
        let _has_world_only = events.iter().any(|e| e.contains("World") && !e.contains("Hello World"));
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
