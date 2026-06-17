//! Kiro EventStream → Anthropic SSE streaming conversion.
//!
//! Reads AWS EventStream binary frames from the upstream Kiro API,
//! parses events, and converts them to Anthropic Messages SSE format.
//!
//! Split into focused submodules:
//! - [`state`] — `AnthropicStreamState` machine, SSE helpers, per-event processing
//! - [`handler`] — direct and buffered streaming response handlers

mod handler;
mod state;

pub(crate) use handler::{
    handle_stream_anthropic_output, handle_stream_anthropic_output_buffered,
};

// ---- SSE keep-alive constant ----

/// Raw bytes sent as an SSE keep-alive comment (ignored by compliant parsers).
pub(crate) const KEEP_ALIVE_BYTES: &[u8] = b": keepalive\n\n";

// ---- Shared helpers (also used by stream_openai) ----

pub(crate) fn generate_id() -> String {
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
