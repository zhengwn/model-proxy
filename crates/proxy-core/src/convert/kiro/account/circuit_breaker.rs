//! Per-account circuit breaker with exponential backoff.
//!
//! The breaker's fields are `pub(super)` because the `AccountManager` scoring
//! and snapshot logic reads them directly.

use std::time::{Duration, Instant};

use super::ErrorClass;

/// Circuit breaker states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CircuitState {
    /// Account is healthy and available
    Active,
    /// Account has failed, waiting for recovery timeout
    Broken,
    /// Recovery timeout expired, allowing probe requests
    HalfOpen,
}

/// Per-account circuit breaker with exponential backoff.
pub(super) struct AccountCircuitBreaker {
    pub(super) state: CircuitState,
    pub(super) failures: u32,
    pub(super) last_failure: Option<Instant>,
    pub(super) total_requests: u64,
    pub(super) successful_requests: u64,
    pub(super) failed_requests: u64,
    /// Classification of the last error (for self-healing decisions).
    pub(super) last_error_class: Option<ErrorClass>,
    /// Health score 0-100, starts at 100
    pub(super) health_score: u32,
}

impl AccountCircuitBreaker {
    pub(super) fn new() -> Self {
        Self {
            state: CircuitState::Active,
            failures: 0,
            last_failure: None,
            total_requests: 0,
            successful_requests: 0,
            failed_requests: 0,
            last_error_class: None,
            health_score: 100,
        }
    }

    /// Check if the account is available for use.
    pub(super) fn is_available(&self) -> bool {
        match self.state {
            CircuitState::Active => true,
            CircuitState::Broken => {
                // Check if recovery timeout has expired
                if let Some(last) = self.last_failure {
                    let backoff = self.recovery_timeout();
                    last.elapsed() >= backoff
                } else {
                    true
                }
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Recovery timeout with exponential backoff.
    /// Starts at 60 seconds, doubles each failure, caps at 1 day.
    pub(super) fn recovery_timeout(&self) -> Duration {
        const BASE_TIMEOUT: Duration = Duration::from_secs(60);
        const MAX_MULTIPLIER: u64 = 1440; // 60s * 1440 = 1 day
        let multiplier = 2u64.saturating_pow(self.failures.saturating_sub(1)).min(MAX_MULTIPLIER);
        BASE_TIMEOUT * multiplier as u32
    }

    /// Record a successful request.
    pub(super) fn record_success(&mut self, recovery: u32) {
        self.total_requests += 1;
        self.successful_requests += 1;
        self.failures = 0;
        self.state = CircuitState::Active;
        self.health_score = (self.health_score + recovery).min(100);
    }

    /// Record a failed request (recoverable error).
    pub(super) fn record_failure(&mut self, error_class: ErrorClass, decay: u32) {
        self.total_requests += 1;
        self.failed_requests += 1;
        self.failures += 1;
        self.last_failure = Some(Instant::now());
        self.last_error_class = Some(error_class.clone());
        self.state = CircuitState::Broken;
        let penalty = match error_class {
            ErrorClass::Recoverable => decay,
            ErrorClass::Suspended | ErrorClass::Fatal => decay * 2,
        };
        self.health_score = self.health_score.saturating_sub(penalty);
    }
}
