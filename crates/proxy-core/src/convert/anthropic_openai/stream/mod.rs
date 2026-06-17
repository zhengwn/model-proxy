//! Streaming conversion between OpenAI and Anthropic SSE formats.
//!
//! Split into focused submodules:
//! - [`common`] — shared SSE framing, block tracking, usage normalization
//! - [`log_context`] — `StreamLogContext` request logging
//! - [`openai_to_anthropic`] — OpenAI Chat Completions SSE → Anthropic Messages SSE
//! - [`anthropic_to_openai`] — Anthropic Messages SSE → OpenAI Chat Completions SSE
//!
//! All public items are re-exported here so callers continue to use the
//! `convert::anthropic_openai::stream::*` paths unchanged.

mod anthropic_to_openai;
mod common;
mod log_context;
mod openai_to_anthropic;

// Items used by other modules in all build configurations.
pub(crate) use anthropic_to_openai::handle_stream_openai_output;
pub(crate) use common::{build_anthropic_usage, extract_openai_usage_parts};
pub(crate) use log_context::StreamLogContext;
pub(crate) use openai_to_anthropic::handle_stream;

// Items exercised only by the sibling `tests` module. Re-exported under
// `#[cfg(test)]` so non-test builds don't flag them as unused.
#[cfg(test)]
pub(crate) use anthropic_to_openai::{convert_anthropic_stream_chunk, OpenAiStreamOutputState};
#[cfg(test)]
pub(crate) use common::{compact_sse_buffer, next_sse_block};
#[cfg(test)]
pub(crate) use openai_to_anthropic::{convert_stream_chunk, StreamConversionState};

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
