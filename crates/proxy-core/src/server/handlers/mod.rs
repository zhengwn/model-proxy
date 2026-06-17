//! HTTP request handlers for the proxy server.
//!
//! Split into focused submodules by endpoint group:
//! - [`messages`] — `POST /v1/messages` (Anthropic Messages API)
//! - [`completions`] — `POST /v1/chat/completions` (OpenAI Chat Completions)
//! - [`models`] — `/v1/models`, `/v1/messages/count_tokens`, `/v1/responses`
//! - [`status`] — telemetry intake, `/api/usage`, `/api/flows`, `/api/status`

mod completions;
mod messages;
mod models;
mod status;

pub use completions::proxy_chat_completions;
pub use messages::proxy_messages;
pub use models::{proxy_count_tokens, proxy_models, proxy_responses};
pub use status::{event_logging_batch, proxy_flows, proxy_status, proxy_usage};

use crate::convert::utils::truncate_for_log;

/// Sanitize upstream error messages before forwarding to clients.
/// - 4xx errors: forward truncated message (client can fix their request)
/// - 5xx errors: generic message (don't leak internal infrastructure details)
pub(super) fn sanitize_upstream_error(status: u16, body: &str) -> String {
    if (400..500).contains(&status) {
        truncate_for_log(body, 512)
    } else {
        format!("Upstream service error (HTTP {})", status)
    }
}

pub(super) fn now_epoch_secs() -> u64 {
    crate::server::state::now_epoch_secs()
}

/// Record request/error metrics for a completed handler result.
///
/// Centralizes the metrics-recording tail shared by the proxy handlers:
/// a server-error response or an `Err` counts as an error, anything else as a
/// successful request. No-op when metrics are not configured.
pub(super) fn record_result_metrics(
    metrics: Option<&std::sync::Arc<crate::server::metrics::Metrics>>,
    result: &crate::error::Result<axum::response::Response>,
    request_start: std::time::Instant,
) {
    let Some(m) = metrics else { return };
    let elapsed = request_start.elapsed().as_millis() as u64;
    let is_error = match result {
        Ok(resp) => resp.status().is_server_error(),
        Err(_) => true,
    };
    if is_error {
        m.record_error(elapsed);
    } else {
        m.record_request(elapsed);
    }
}
