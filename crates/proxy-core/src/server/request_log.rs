//! Unified request logging utilities.
//!
//! Provides `RequestLog` — a single struct constructed once per request
//! that handles all logging lifecycle events (error, success, stream completion).
//! Eliminates repeated `LogContext` + `emit_log_entry()` boilerplate in handlers.

use std::sync::Arc;
use std::time::Instant;

use crate::convert::anthropic_openai::stream::StreamLogContext;
use crate::logging::{truncate_body, LogCollector, LogEntry};

/// Unified request log context. Constructed once at the beginning of a handler,
/// then used to emit log entries at various lifecycle points.
#[derive(Clone)]
pub(crate) struct RequestLog {
    pub collector: Arc<LogCollector>,
    pub request_id: String,
    pub method: &'static str,
    pub path: &'static str,
    pub provider: String,
    pub model: String,
    pub requested_model: String,
    pub request_start: Instant,
    pub upstream_start: Instant,
    pub is_stream: bool,
    pub raw_request_body: String,
}

impl RequestLog {
    /// Update the upstream_start timestamp (call just before sending upstream request).
    pub fn mark_upstream_start(&mut self) {
        self.upstream_start = Instant::now();
    }

    /// Emit a log entry for a completed request (success or error).
    pub fn emit(
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
        let duration_ms = self.request_start.elapsed().as_millis() as u64;
        let proxy_overhead_ms = self
            .upstream_start
            .duration_since(self.request_start)
            .as_millis() as u64;

        let entry = LogEntry {
            id: self.request_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            method: self.method.to_string(),
            path: self.path.to_string(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            requested_model: Some(self.requested_model.clone()),
            status,
            duration_ms,
            proxy_overhead_ms: Some(proxy_overhead_ms),
            ttft_ms,
            error_message,
            request_body: if log_config.record_body {
                Some(truncate_body(&self.raw_request_body, log_config.max_body_bytes))
            } else {
                None
            },
            response_body: if log_config.record_body {
                response_body.map(|b| truncate_body(b, log_config.max_body_bytes))
            } else {
                None
            },
            is_stream: self.is_stream,
            token_count: None,
        };
        self.collector.emit(entry);
    }

    /// Emit a log entry for a network/send error (502).
    pub fn emit_send_error(&self, error: &str) {
        self.emit(502, None, Some(error.to_string()), None);
    }

    /// Emit a log entry for a successful non-stream response.
    pub fn emit_success(&self, upstream_headers_ms: u128) {
        self.emit(200, Some(upstream_headers_ms as u64), None, None);
    }

    /// Emit a log entry for an upstream error response.
    pub fn emit_upstream_error(&self, status_code: u16, upstream_headers_ms: u128, body: &str) {
        let truncated = crate::convert::utils::truncate_for_log(body, super::state::MAX_LOG_BODY_BYTES);
        self.emit(
            status_code,
            Some(upstream_headers_ms as u64),
            Some(truncated.clone()),
            Some(body),
        );
    }

    /// Convert to a `StreamLogContext` for passing into stream response handlers.
    /// The stream handler will call `.emit()` on completion.
    pub fn to_stream_log_ctx(&self) -> StreamLogContext {
        StreamLogContext {
            collector: self.collector.clone(),
            request_id: self.request_id.clone(),
            method: self.method,
            path: self.path,
            provider: self.provider.clone(),
            model: self.model.clone(),
            requested_model: self.requested_model.clone(),
            request_start: self.request_start,
            upstream_start: self.upstream_start,
            raw_request_body: self.raw_request_body.clone(),
        }
    }
}
