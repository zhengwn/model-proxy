//! Endpoint health tracking for upstream Kiro endpoints.
//!
//! Tracks per-endpoint success/failure counts, exponential moving average latency,
//! consecutive error streaks, and timestamps of last success/error.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

const EMA_ALPHA: f64 = 0.3;

/// Per-endpoint health statistics.
#[derive(Debug, Clone)]
pub struct EndpointStats {
    pub success_count: u64,
    pub fail_count: u64,
    pub latency_ema: f64,
    pub consecutive_errors: u32,
    pub last_success: Option<Instant>,
    pub last_error: Option<Instant>,
}

impl EndpointStats {
    fn new() -> Self {
        Self {
            success_count: 0,
            fail_count: 0,
            latency_ema: 0.0,
            consecutive_errors: 0,
            last_success: None,
            last_error: None,
        }
    }
}

/// Serializable snapshot of all endpoint stats.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthSnapshot {
    pub endpoints: Vec<EndpointSnapshot>,
}

/// Serializable snapshot of a single endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EndpointSnapshot {
    pub endpoint: String,
    pub success_count: u64,
    pub fail_count: u64,
    pub latency_ema_ms: f64,
    pub consecutive_errors: u32,
    pub success_rate: f64,
}

/// Thread-safe tracker for upstream endpoint health.
#[derive(Clone)]
pub struct EndpointHealthTracker {
    inner: Arc<RwLock<HashMap<String, EndpointStats>>>,
}

impl EndpointHealthTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record a successful request to the given endpoint.
    pub fn record_success(&self, endpoint: &str, latency_ms: f64) {
        let mut inner = self.inner.write().unwrap();
        let stats = inner
            .entry(endpoint.to_string())
            .or_insert_with(EndpointStats::new);

        stats.success_count += 1;
        stats.consecutive_errors = 0;
        stats.last_success = Some(Instant::now());

        // Update EMA
        if stats.latency_ema == 0.0 {
            stats.latency_ema = latency_ms;
        } else {
            stats.latency_ema = EMA_ALPHA * latency_ms + (1.0 - EMA_ALPHA) * stats.latency_ema;
        }
    }

    /// Record a failed request to the given endpoint.
    pub fn record_failure(&self, endpoint: &str) {
        let mut inner = self.inner.write().unwrap();
        let stats = inner
            .entry(endpoint.to_string())
            .or_insert_with(EndpointStats::new);

        stats.fail_count += 1;
        stats.consecutive_errors += 1;
        stats.last_error = Some(Instant::now());
    }

    /// Return a serializable snapshot of all endpoint health data.
    pub fn snapshot(&self) -> HealthSnapshot {
        let inner = self.inner.read().unwrap();
        let endpoints = inner
            .iter()
            .map(|(endpoint, stats)| {
                let total = stats.success_count + stats.fail_count;
                let success_rate = if total > 0 {
                    stats.success_count as f64 / total as f64
                } else {
                    0.0
                };
                EndpointSnapshot {
                    endpoint: endpoint.clone(),
                    success_count: stats.success_count,
                    fail_count: stats.fail_count,
                    latency_ema_ms: (stats.latency_ema * 100.0).round() / 100.0,
                    consecutive_errors: stats.consecutive_errors,
                    success_rate: (success_rate * 10000.0).round() / 10000.0,
                }
            })
            .collect();
        HealthSnapshot { endpoints }
    }
}

impl Default for EndpointHealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_success_updates_stats() {
        let tracker = EndpointHealthTracker::new();
        tracker.record_success("us-east-1", 150.0);
        tracker.record_success("us-east-1", 200.0);

        let snap = tracker.snapshot();
        assert_eq!(snap.endpoints.len(), 1);
        let ep = &snap.endpoints[0];
        assert_eq!(ep.endpoint, "us-east-1");
        assert_eq!(ep.success_count, 2);
        assert_eq!(ep.fail_count, 0);
        assert_eq!(ep.consecutive_errors, 0);
    }

    #[test]
    fn record_failure_increments_consecutive_errors() {
        let tracker = EndpointHealthTracker::new();
        tracker.record_failure("eu-west-1");
        tracker.record_failure("eu-west-1");

        let snap = tracker.snapshot();
        let ep = &snap.endpoints[0];
        assert_eq!(ep.consecutive_errors, 2);
        assert_eq!(ep.fail_count, 2);
    }

    #[test]
    fn success_resets_consecutive_errors() {
        let tracker = EndpointHealthTracker::new();
        tracker.record_failure("us-west-2");
        tracker.record_failure("us-west-2");
        tracker.record_success("us-west-2", 100.0);

        let snap = tracker.snapshot();
        let ep = &snap.endpoints[0];
        assert_eq!(ep.consecutive_errors, 0);
        assert_eq!(ep.success_count, 1);
        assert_eq!(ep.fail_count, 2);
    }

    #[test]
    fn latency_ema_converges() {
        let tracker = EndpointHealthTracker::new();
        for _ in 0..100 {
            tracker.record_success("endpoint", 100.0);
        }
        let snap = tracker.snapshot();
        let ep = &snap.endpoints[0];
        // EMA should converge toward 100.0
        assert!((ep.latency_ema_ms - 100.0).abs() < 1.0);
    }
}
