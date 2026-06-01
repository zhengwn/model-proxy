//! Lightweight Prometheus-style metrics without the prometheus crate.
//!
//! Uses atomic counters and exposes a `/metrics` endpoint in Prometheus text format.
//! Designed for minimal overhead and no external dependencies.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Duration histogram bucket boundaries in seconds.
const HISTOGRAM_BUCKETS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];
/// Number of histogram buckets (each bucket + the +Inf catch-all).
const NUM_BUCKETS: usize = HISTOGRAM_BUCKETS.len() + 1; // 11 buckets

/// Collects proxy-level metrics using lock-free atomic counters.
///
/// All fields are `AtomicU64` for thread-safe concurrent access.
/// The struct is intended to be wrapped in `Arc<Metrics>` and shared across
/// request handlers and the `/metrics` endpoint.
pub struct Metrics {
    // ---- Counters (monotonically increasing) ----
    /// Total number of requests received by the proxy.
    pub requests_total: AtomicU64,
    /// Total number of requests that resulted in an error (4xx/5xx from upstream or proxy).
    pub errors_total: AtomicU64,
    /// Total number of automatic retries performed.
    pub retries_total: AtomicU64,
    /// Total input (prompt) tokens consumed across all requests.
    pub tokens_input: AtomicU64,
    /// Total output (completion) tokens produced across all requests.
    pub tokens_output: AtomicU64,

    // ---- Gauges ----
    /// Number of requests currently being processed.
    pub active_connections: AtomicU64,

    // ---- Histogram (request duration in seconds) ----
    /// Per-bucket counters for request duration.
    /// Index 0..HISTOGRAM_BUCKETS.len() corresponds to the defined thresholds.
    /// The last bucket (`+Inf`) captures all requests.
    buckets: [AtomicU64; NUM_BUCKETS],
    /// Sum of all observed durations (in milliseconds, stored as u64 to avoid floats).
    histogram_sum_ms: AtomicU64,
    /// Total number of observations (equals `buckets[NUM_BUCKETS-1]`).
    histogram_count: AtomicU64,

    // ---- Bookkeeping ----
    /// Monotonic instant when the metrics object was created (for `process_start_seconds`).
    start: Instant,
}

impl Metrics {
    /// Create a new metrics instance, recording the current time as the start.
    pub fn new() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            retries_total: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            histogram_sum_ms: AtomicU64::new(0),
            histogram_count: AtomicU64::new(0),
            start: Instant::now(),
        }
    }

    // ---- Convenience helpers ----

    /// Record a completed request: increments `requests_total` and records its duration.
    pub fn record_request(&self, duration_ms: u64) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.observe_duration(duration_ms);
    }

    /// Record an error: increments `errors_total` in addition to `requests_total`.
    pub fn record_error(&self, duration_ms: u64) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
        self.record_request(duration_ms);
    }

    /// Increment the retry counter.
    pub fn record_retry(&self) {
        self.retries_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Add token usage from a completed request.
    pub fn record_tokens(&self, input: u64, output: u64) {
        self.tokens_input.fetch_add(input, Ordering::Relaxed);
        self.tokens_output.fetch_add(output, Ordering::Relaxed);
    }

    /// Increment active connections (call at request start).
    pub fn connection_start(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement active connections (call at request end).
    pub fn connection_end(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    /// Observe a duration in milliseconds, placing it into the correct histogram bucket.
    fn observe_duration(&self, duration_ms: u64) {
        let secs = duration_ms as f64 / 1000.0;
        let mut placed = false;
        for (i, &threshold) in HISTOGRAM_BUCKETS.iter().enumerate() {
            if secs <= threshold {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                placed = true;
                break;
            }
        }
        if !placed {
            // Falls into the +Inf bucket (last index).
            self.buckets[NUM_BUCKETS - 1].fetch_add(1, Ordering::Relaxed);
        }
        self.histogram_sum_ms.fetch_add(duration_ms, Ordering::Relaxed);
        self.histogram_count.fetch_add(1, Ordering::Relaxed);
    }

    // ---- Prometheus text format rendering ----

    /// Render all metrics in Prometheus exposition text format.
    ///
    /// Format follows the OpenMetrics/Prometheus text spec:
    /// - Counters end with `_total`
    /// - Histograms have `_bucket{le="..."}`, `_sum`, and `_count` suffixes
    /// - A trailing `# EOF` is included for strict parsers
    pub fn render(&self) -> String {
        let uptime_secs = self.start.elapsed().as_secs_f64();
        let mut out = String::with_capacity(2048);

        // HELP / TYPE headers for counters
        out.push_str("# HELP proxy_requests_total Total number of requests received.\n");
        out.push_str("# TYPE proxy_requests_total counter\n");
        out.push_str(&format!("proxy_requests_total {}\n", self.requests_total.load(Ordering::Relaxed)));

        out.push_str("# HELP proxy_errors_total Total number of error responses.\n");
        out.push_str("# TYPE proxy_errors_total counter\n");
        out.push_str(&format!("proxy_errors_total {}\n", self.errors_total.load(Ordering::Relaxed)));

        out.push_str("# HELP proxy_retries_total Total number of automatic retries.\n");
        out.push_str("# TYPE proxy_retries_total counter\n");
        out.push_str(&format!("proxy_retries_total {}\n", self.retries_total.load(Ordering::Relaxed)));

        out.push_str("# HELP proxy_tokens_input_total Total input tokens consumed.\n");
        out.push_str("# TYPE proxy_tokens_input_total counter\n");
        out.push_str(&format!("proxy_tokens_input_total {}\n", self.tokens_input.load(Ordering::Relaxed)));

        out.push_str("# HELP proxy_tokens_output_total Total output tokens produced.\n");
        out.push_str("# TYPE proxy_tokens_output_total counter\n");
        out.push_str(&format!("proxy_tokens_output_total {}\n", self.tokens_output.load(Ordering::Relaxed)));

        // Gauges
        out.push_str("# HELP proxy_active_connections Number of requests currently in-flight.\n");
        out.push_str("# TYPE proxy_active_connections gauge\n");
        out.push_str(&format!("proxy_active_connections {}\n", self.active_connections.load(Ordering::Relaxed)));

        out.push_str("# HELP proxy_uptime_seconds Time in seconds since the proxy started.\n");
        out.push_str("# TYPE proxy_uptime_seconds gauge\n");
        out.push_str(&format!("proxy_uptime_seconds {:.6}\n", uptime_secs));

        // Histogram: request_duration_seconds
        out.push_str("# HELP proxy_request_duration_seconds Request duration histogram in seconds.\n");
        out.push_str("# TYPE proxy_request_duration_seconds histogram\n");
        for (i, &threshold) in HISTOGRAM_BUCKETS.iter().enumerate() {
            let cumulative = self.cumulative_bucket_count(i);
            out.push_str(&format!(
                "proxy_request_duration_seconds_bucket{{le=\"{:.1}\"}} {}\n",
                threshold, cumulative,
            ));
        }
        // +Inf bucket (always equals histogram_count)
        let total = self.histogram_count.load(Ordering::Relaxed);
        out.push_str(&format!("proxy_request_duration_seconds_bucket{{le=\"+Inf\"}} {}\n", total));

        let sum_secs = self.histogram_sum_ms.load(Ordering::Relaxed) as f64 / 1000.0;
        out.push_str(&format!("proxy_request_duration_seconds_sum {:.6}\n", sum_secs));
        out.push_str(&format!("proxy_request_duration_seconds_count {}\n", total));

        out.push_str("# EOF\n");
        out
    }

    /// Compute the cumulative count for a histogram bucket (sum of buckets 0..=idx).
    fn cumulative_bucket_count(&self, idx: usize) -> u64 {
        (0..=idx)
            .map(|i| self.buckets[i].load(Ordering::Relaxed))
            .sum()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_metrics_has_zero_counts() {
        let m = Metrics::new();
        assert_eq!(m.requests_total.load(Ordering::Relaxed), 0);
        assert_eq!(m.errors_total.load(Ordering::Relaxed), 0);
        assert_eq!(m.retries_total.load(Ordering::Relaxed), 0);
        assert_eq!(m.tokens_input.load(Ordering::Relaxed), 0);
        assert_eq!(m.tokens_output.load(Ordering::Relaxed), 0);
        assert_eq!(m.active_connections.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn record_request_increments_total() {
        let m = Metrics::new();
        m.record_request(150);
        assert_eq!(m.requests_total.load(Ordering::Relaxed), 1);
        assert_eq!(m.histogram_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_error_increments_both() {
        let m = Metrics::new();
        m.record_error(500);
        assert_eq!(m.requests_total.load(Ordering::Relaxed), 1);
        assert_eq!(m.errors_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn histogram_bucket_placement() {
        let m = Metrics::new();
        // 50ms → 0.1s bucket
        m.observe_duration(50);
        assert_eq!(m.buckets[0].load(Ordering::Relaxed), 1);

        // 500ms → 0.5s bucket
        m.observe_duration(500);
        assert_eq!(m.buckets[2].load(Ordering::Relaxed), 1); // 0.5 is index 2

        // 3500ms → 5.0s bucket (index 5: [0.1, 0.25, 0.5, 1.0, 2.5, 5.0, ...])
        m.observe_duration(3500);
        assert_eq!(m.buckets[5].load(Ordering::Relaxed), 1);

        // 200_000ms (200s) → +Inf bucket (last index)
        m.observe_duration(200_000);
        assert_eq!(m.buckets[NUM_BUCKETS - 1].load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cumulative_bucket_count_works() {
        let m = Metrics::new();
        m.observe_duration(50);   // bucket 0
        m.observe_duration(50);   // bucket 0
        m.observe_duration(300);  // bucket 2 (0.5)
        m.observe_duration(500);  // bucket 2 (0.5)

        // cumulative at bucket 0 = 2
        assert_eq!(m.cumulative_bucket_count(0), 2);
        // cumulative at bucket 1 = 2 (bucket 1 is empty, but 0..=1)
        assert_eq!(m.cumulative_bucket_count(1), 2);
        // cumulative at bucket 2 = 4
        assert_eq!(m.cumulative_bucket_count(2), 4);
    }

    #[test]
    fn render_contains_expected_lines() {
        let m = Metrics::new();
        m.record_request(100);
        m.record_error(200);
        m.record_retry();
        m.record_tokens(500, 200);
        m.connection_start();

        let text = m.render();
        assert!(text.contains("proxy_requests_total 2"));
        assert!(text.contains("proxy_errors_total 1"));
        assert!(text.contains("proxy_retries_total 1"));
        assert!(text.contains("proxy_tokens_input_total 500"));
        assert!(text.contains("proxy_tokens_output_total 200"));
        assert!(text.contains("proxy_active_connections 1"));
        assert!(text.contains("proxy_uptime_seconds"));
        assert!(text.contains("proxy_request_duration_seconds_bucket"));
        assert!(text.contains("# EOF"));
    }
}
