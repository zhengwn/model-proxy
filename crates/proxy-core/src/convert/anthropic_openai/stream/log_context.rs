//! Streaming request log context shared by both conversion directions.

use std::sync::Arc;
use std::time::Instant;

use crate::server::state::elapsed_ms;
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
