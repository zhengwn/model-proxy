//! Shared helpers for streaming conversion: SSE framing, block tracking,
//! and OpenAI/Anthropic usage normalization. Used by both conversion directions.

use serde_json::{json, Value};
use smallvec::SmallVec;
use std::collections::BTreeSet;

use crate::convert::utils::find_sse_block_end;

// ---- SSE formatting helpers (hot path) ----
// These helpers centralize SSE framing, pre-allocate the output string, and serialize
// payloads compactly. Static payloads can use `sse_event_literal` to skip serde_json.

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

/// For tiny, static-structure payloads we skip `serde_json` entirely.
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
    let index = index.to_string();
    let json_len = r#"{"type":"content_block_stop","index":}"#.len() + index.len();
    let mut s = String::with_capacity(8 + "content_block_stop".len() + 8 + json_len + 2);
    s.push_str("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":");
    s.push_str(&index);
    s.push_str("}\n\n");
    s
}

pub(super) type StreamEvents = SmallVec<[String; 4]>;

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

pub(super) fn next_stream_block_index(next_content_block_index: &mut usize) -> usize {
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

pub(super) fn push_content_block_stop(events: &mut StreamEvents, index: usize) {
    events.push(sse_content_block_stop(index));
}

pub(super) fn stop_current_stream_block(
    events: &mut StreamEvents,
    current_block: &mut Option<CurrentBlock>,
) {
    if let Some(block) = current_block.take() {
        push_content_block_stop(events, block.index);
    }
}

pub(super) fn close_open_tool_blocks(
    events: &mut StreamEvents,
    open_tool_blocks: &mut BTreeSet<usize>,
) {
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

/// Current epoch seconds; used to stamp OpenAI chunk `created` fields.
pub(super) fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
