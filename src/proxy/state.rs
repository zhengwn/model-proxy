use reqwest::Client;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::info;

use crate::config::Config;

pub(crate) const ANTHROPIC_BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";
pub(crate) const MAX_LOG_BODY_BYTES: usize = 4096;
pub(crate) const UPSTREAM_CONNECT_TIMEOUT_SECS: u64 = 30;
pub(crate) const NON_STREAM_REQUEST_TIMEOUT_SECS: u64 = 300;
pub(crate) const UPSTREAM_POOL_IDLE_TIMEOUT_SECS: u64 = 90;
pub(crate) const UPSTREAM_POOL_MAX_IDLE_PER_HOST: usize = 32;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_request_id() -> String {
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("req_{}_{}", millis, counter)
}

pub(crate) fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

pub(crate) struct RequestCompletionGuard {
    request_id: String,
    request_start: Instant,
    phase: &'static str,
    completed: bool,
}

impl RequestCompletionGuard {
    pub(crate) fn new(request_id: String, request_start: Instant) -> Self {
        Self {
            request_id,
            request_start,
            phase: "received",
            completed: false,
        }
    }

    pub(crate) fn set_phase(&mut self, phase: &'static str) {
        self.phase = phase;
    }

    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RequestCompletionGuard {
    fn drop(&mut self) {
        if !self.completed {
            info!(
                request_id = self.request_id.as_str(),
                phase = self.phase,
                request_total_ms = elapsed_ms(self.request_start),
                "请求处理提前结束"
            );
        }
    }
}

pub(crate) fn strip_leading_anthropic_billing_header(text: &str) -> &str {
    if !text.starts_with(ANTHROPIC_BILLING_HEADER_PREFIX) {
        return text;
    }
    let Some(line_end) = text
        .as_bytes()
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
    else {
        return "";
    };
    let bytes = text.as_bytes();
    let mut rest_start = line_end + 1;
    if bytes[line_end] == b'\r' && bytes.get(line_end + 1) == Some(&b'\n') {
        rest_start += 1;
    }
    &text[rest_start..]
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: Client,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(UPSTREAM_CONNECT_TIMEOUT_SECS))
            .pool_idle_timeout(Duration::from_secs(UPSTREAM_POOL_IDLE_TIMEOUT_SECS))
            .pool_max_idle_per_host(UPSTREAM_POOL_MAX_IDLE_PER_HOST)
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .expect("构建 HTTP 客户端失败");

        Self {
            config: Arc::new(config),
            client,
        }
    }
}
