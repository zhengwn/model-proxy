//! Multi-account failover for Kiro API with circuit breaker pattern.
//!
//! Manages multiple Kiro credentials with automatic failover when an account
//! encounters recoverable errors (403, 429, 402). Uses exponential backoff
//! with probabilistic retry for broken accounts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::auth::KiroAuthManager;
use crate::config::KiroConfig;

/// Error classification for failover decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorClass {
    /// Account-specific error, try next account
    Recoverable,
    /// Request-level error, return to client immediately
    Fatal,
}

/// Classify an HTTP status code for failover decisions.
pub fn classify_error(status: u16, body: &str) -> ErrorClass {
    match status {
        402 => ErrorClass::Recoverable, // Quota exceeded
        403 => ErrorClass::Recoverable, // Token expired/invalid
        429 => ErrorClass::Recoverable, // Rate limit
        400 => {
            if body.contains("INVALID_MODEL_ID") {
                ErrorClass::Recoverable // Model not on this tier
            } else if body.contains("CONTENT_LENGTH_EXCEEDS") {
                ErrorClass::Fatal
            } else {
                ErrorClass::Fatal // Malformed request
            }
        }
        422 => ErrorClass::Fatal, // Validation error
        _ if status >= 500 => ErrorClass::Fatal, // Server error
        _ => ErrorClass::Fatal,
    }
}

/// Circuit breaker states.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CircuitState {
    /// Account is healthy and available
    Active,
    /// Account has failed, waiting for recovery timeout
    Broken,
    /// Recovery timeout expired, allowing probe requests
    HalfOpen,
}

/// Per-account circuit breaker with exponential backoff.
struct AccountCircuitBreaker {
    state: CircuitState,
    failures: u32,
    last_failure: Option<Instant>,
    total_requests: u64,
    successful_requests: u64,
    failed_requests: u64,
}

impl AccountCircuitBreaker {
    fn new() -> Self {
        Self {
            state: CircuitState::Active,
            failures: 0,
            last_failure: None,
            total_requests: 0,
            successful_requests: 0,
            failed_requests: 0,
        }
    }

    /// Check if the account is available for use.
    fn is_available(&self) -> bool {
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
    fn recovery_timeout(&self) -> Duration {
        const BASE_TIMEOUT: Duration = Duration::from_secs(60);
        const MAX_MULTIPLIER: u64 = 1440; // 60s * 1440 = 1 day
        let multiplier = 2u64.saturating_pow(self.failures.saturating_sub(1)).min(MAX_MULTIPLIER);
        BASE_TIMEOUT * multiplier as u32
    }

    /// Record a successful request.
    fn record_success(&mut self) {
        self.total_requests += 1;
        self.successful_requests += 1;
        self.failures = 0;
        self.state = CircuitState::Active;
    }

    /// Record a failed request (recoverable error).
    fn record_failure(&mut self) {
        self.total_requests += 1;
        self.failed_requests += 1;
        self.failures += 1;
        self.last_failure = Some(Instant::now());
        self.state = CircuitState::Broken;
    }
}

/// A managed Kiro account with auth manager and circuit breaker.
struct ManagedAccount {
    id: String,
    auth: Arc<Mutex<KiroAuthManager>>,
    circuit: AccountCircuitBreaker,
    /// Optional per-account proxy URL (HTTP/SOCKS5)
    proxy_url: Option<String>,
    /// Priority (lower = higher priority)
    priority: u32,
    /// Whether this account is manually disabled
    disabled: bool,
}

/// Load balancing mode for account selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadBalancingMode {
    /// Sticky to current account, fall back on failure
    Priority,
    /// Round-robin across available accounts
    Balanced,
}

impl LoadBalancingMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "balanced" => Self::Balanced,
            _ => Self::Priority,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Balanced => "balanced",
        }
    }
}

/// Snapshot of a single account's status for the Admin API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountSnapshot {
    pub id: String,
    pub priority: u32,
    pub disabled: bool,
    pub failure_count: u32,
    pub is_current: bool,
    pub is_available: bool,
    pub auth_method: String,
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub proxy_url: Option<String>,
    pub region: String,
}

/// Multi-account manager with circuit breaker failover.
pub struct AccountManager {
    accounts: Vec<ManagedAccount>,
    current_index: usize,
    /// Probability of retrying a broken account (0.0 - 1.0)
    probabilistic_retry_chance: f64,
    /// Load balancing mode
    load_balancing_mode: LoadBalancingMode,
    /// Round-robin counter for balanced mode
    round_robin_index: usize,
}

impl AccountManager {
    /// Create a new account manager from multiple Kiro configs.
    pub fn new(configs: &[(String, KiroConfig)], client: reqwest::Client) -> Self {
        Self::new_with_mode(configs, client, LoadBalancingMode::Priority)
    }

    /// Create with explicit load balancing mode.
    pub fn new_with_mode(
        configs: &[(String, KiroConfig)],
        client: reqwest::Client,
        mode: LoadBalancingMode,
    ) -> Self {
        let mut accounts: Vec<ManagedAccount> = configs
            .iter()
            .map(|(id, config)| {
                let auth = KiroAuthManager::new(config, client.clone());
                ManagedAccount {
                    id: id.clone(),
                    auth: Arc::new(Mutex::new(auth)),
                    circuit: AccountCircuitBreaker::new(),
                    proxy_url: config.proxy_url.clone(),
                    priority: 0,
                    disabled: false,
                }
            })
            .collect();

        // Sort by priority (lower = higher priority)
        accounts.sort_by_key(|a| a.priority);

        info!(
            account_count = accounts.len(),
            mode = mode.as_str(),
            "初始化 Kiro 多账户管理器"
        );

        Self {
            accounts,
            current_index: 0,
            probabilistic_retry_chance: 0.1,
            load_balancing_mode: mode,
            round_robin_index: 0,
        }
    }

    /// Get the current preferred account's auth manager.
    /// Returns (account_id, auth_arc).
    pub fn current_account(&self) -> Option<(&str, &Arc<Mutex<KiroAuthManager>>)> {
        self.accounts
            .get(self.current_index)
            .filter(|a| !a.disabled && a.circuit.is_available())
            .map(|a| (a.id.as_str(), &a.auth))
    }

    /// Get an available account, skipping excluded accounts.
    /// Implements sticky behavior: prefers the current account.
    pub fn get_available_account(
        &mut self,
        exclude: &[String],
    ) -> Option<(&str, &Arc<Mutex<KiroAuthManager>>)> {
        match self.load_balancing_mode {
            LoadBalancingMode::Priority => self.get_available_priority(exclude),
            LoadBalancingMode::Balanced => self.get_available_balanced(exclude),
        }
    }

    fn get_available_priority(
        &mut self,
        exclude: &[String],
    ) -> Option<(&str, &Arc<Mutex<KiroAuthManager>>)> {
        // Try current account first (sticky)
        if self.current_index < self.accounts.len() {
            let current = &self.accounts[self.current_index];
            if !current.disabled && current.circuit.is_available() && !exclude.contains(&current.id) {
                return Some((current.id.as_str(), &current.auth));
            }
        }

        // Find next available account index (sorted by priority)
        let mut found_idx = None;
        for (i, account) in self.accounts.iter().enumerate() {
            if i == self.current_index {
                continue;
            }
            if account.disabled || exclude.contains(&account.id) {
                continue;
            }

            if account.circuit.is_available() {
                found_idx = Some(i);
                break;
            }

            // Probabilistic retry for broken accounts
            if account.circuit.state == CircuitState::Broken {
                if self.should_probabilistic_retry(i) {
                    found_idx = Some(i);
                    break;
                }
            }
        }

        if let Some(idx) = found_idx {
            self.current_index = idx;
            let account = &self.accounts[idx];
            Some((account.id.as_str(), &account.auth))
        } else {
            None
        }
    }

    fn get_available_balanced(
        &mut self,
        exclude: &[String],
    ) -> Option<(&str, &Arc<Mutex<KiroAuthManager>>)> {
        let count = self.accounts.len();
        let mut found_idx = None;
        for _ in 0..count {
            let idx = self.round_robin_index % count;
            self.round_robin_index = (self.round_robin_index + 1) % count;
            let account = &self.accounts[idx];
            if account.disabled || exclude.contains(&account.id) {
                continue;
            }
            if account.circuit.is_available() {
                found_idx = Some(idx);
                break;
            }
            // Probabilistic retry for broken accounts
            if account.circuit.state == CircuitState::Broken {
                if self.should_probabilistic_retry(idx) {
                    found_idx = Some(idx);
                    break;
                }
            }
        }

        if let Some(idx) = found_idx {
            self.current_index = idx;
            let account = &self.accounts[idx];
            Some((account.id.as_str(), &account.auth))
        } else {
            None
        }
    }

    /// Check if a broken account should be probabilistically retried.
    fn should_probabilistic_retry(&self, idx: usize) -> bool {
        let account = &self.accounts[idx];
        let elapsed = account
            .circuit
            .last_failure
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let timeout = account.circuit.recovery_timeout().as_secs_f64();
        let recovery_progress = elapsed / timeout;

        if recovery_progress > 0.5 && rand_chance() < self.probabilistic_retry_chance {
            info!(
                account_id = account.id.as_str(),
                recovery_progress,
                "概率重试 broken 账户"
            );
            true
        } else {
            false
        }
    }

    /// Record a successful request for the current account.
    pub fn record_success(&mut self) {
        if let Some(account) = self.accounts.get_mut(self.current_index) {
            account.circuit.record_success();
        }
    }

    /// Record a failed request for the current account.
    pub fn record_failure(&mut self) {
        if let Some(account) = self.accounts.get_mut(self.current_index) {
            let id = account.id.clone();
            account.circuit.record_failure();
            warn!(
                account_id = id.as_str(),
                failures = account.circuit.failures,
                backoff_secs = account.circuit.recovery_timeout().as_secs(),
                "账户 circuit breaker 触发"
            );
        }
    }

    /// Check if we only have a single account (skip circuit breaker).
    pub fn is_single_account(&self) -> bool {
        self.accounts.iter().filter(|a| !a.disabled).count() <= 1
    }

    /// Get the proxy URL for the current account.
    pub fn current_proxy_url(&self) -> Option<&str> {
        self.accounts
            .get(self.current_index)
            .and_then(|a| a.proxy_url.as_deref())
    }

    /// Get the number of accounts.
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    // ---- Admin API CRUD methods ----

    /// Get a snapshot of all accounts for the Admin API.
    pub fn snapshot(&self) -> Vec<AccountSnapshot> {
        self.accounts
            .iter()
            .enumerate()
            .map(|(i, a)| AccountSnapshot {
                id: a.id.clone(),
                priority: a.priority,
                disabled: a.disabled,
                failure_count: a.circuit.failures,
                is_current: i == self.current_index,
                is_available: !a.disabled && a.circuit.is_available(),
                auth_method: "unknown".to_string(),
                total_requests: a.circuit.total_requests,
                successful_requests: a.circuit.successful_requests,
                failed_requests: a.circuit.failed_requests,
                proxy_url: a.proxy_url.clone(),
                region: String::new(),
            })
            .collect()
    }

    /// Add a new account. Returns the account ID.
    pub fn add_account(
        &mut self,
        id: String,
        config: &KiroConfig,
        client: reqwest::Client,
        priority: u32,
    ) -> String {
        let auth = KiroAuthManager::new(config, client);
        let account_id = id.clone();
        self.accounts.push(ManagedAccount {
            id,
            auth: Arc::new(Mutex::new(auth)),
            circuit: AccountCircuitBreaker::new(),
            proxy_url: config.proxy_url.clone(),
            priority,
            disabled: false,
        });
        // Re-sort by priority
        self.accounts.sort_by_key(|a| a.priority);
        // Update current_index to track the same account
        self.current_index = self
            .accounts
            .iter()
            .position(|a| a.id == self.accounts.get(self.current_index).map(|a| a.id.as_str()).unwrap_or(""))
            .unwrap_or(0);
        info!(account_id = account_id.as_str(), priority, "添加 Kiro 账户");
        account_id
    }

    /// Remove an account by ID. Returns true if found and removed.
    pub fn remove_account(&mut self, id: &str) -> bool {
        if let Some(pos) = self.accounts.iter().position(|a| a.id == id) {
            self.accounts.remove(pos);
            if self.current_index >= self.accounts.len() && !self.accounts.is_empty() {
                self.current_index = self.accounts.len() - 1;
            }
            info!(account_id = id, "删除 Kiro 账户");
            true
        } else {
            false
        }
    }

    /// Set disabled state for an account. Returns true if found.
    pub fn set_disabled(&mut self, id: &str, disabled: bool) -> bool {
        if let Some(account) = self.accounts.iter_mut().find(|a| a.id == id) {
            account.disabled = disabled;
            info!(account_id = id, disabled, "设置 Kiro 账户状态");
            true
        } else {
            false
        }
    }

    /// Set priority for an account and re-sort. Returns true if found.
    pub fn set_priority(&mut self, id: &str, priority: u32) -> bool {
        if let Some(account) = self.accounts.iter_mut().find(|a| a.id == id) {
            let old_priority = account.priority;
            account.priority = priority;
            // Re-sort by priority
            let current_id = self
                .accounts
                .get(self.current_index)
                .map(|a| a.id.clone());
            self.accounts.sort_by_key(|a| a.priority);
            // Restore current_index
            if let Some(cid) = current_id {
                self.current_index = self
                    .accounts
                    .iter()
                    .position(|a| a.id == cid)
                    .unwrap_or(0);
            }
            info!(
                account_id = id,
                old_priority, priority, "设置 Kiro 账户优先级"
            );
            true
        } else {
            false
        }
    }

    /// Reset failure count and re-enable an account. Returns true if found.
    pub fn reset_failures(&mut self, id: &str) -> bool {
        if let Some(account) = self.accounts.iter_mut().find(|a| a.id == id) {
            account.circuit.failures = 0;
            account.circuit.state = CircuitState::Active;
            account.circuit.last_failure = None;
            info!(account_id = id, "重置 Kiro 账户失败计数");
            true
        } else {
            false
        }
    }

    /// Force refresh token for a specific account. Returns Ok(token) or Err.
    pub async fn force_refresh_account(&self, id: &str) -> Result<String, String> {
        if let Some(account) = self.accounts.iter().find(|a| a.id == id) {
            let mut auth = account.auth.lock().await;
            auth.force_refresh()
                .await
                .map_err(|e| format!("Token refresh failed: {}", e))
        } else {
            Err(format!("Account '{}' not found", id))
        }
    }

    /// Get the current load balancing mode.
    pub fn load_balancing_mode(&self) -> &LoadBalancingMode {
        &self.load_balancing_mode
    }

    /// Set the load balancing mode.
    pub fn set_load_balancing_mode(&mut self, mode: LoadBalancingMode) {
        self.load_balancing_mode = mode;
    }
}

/// Simple pseudo-random chance check (0.0 - 1.0).
/// Uses current nanosecond timestamp for randomness.
fn rand_chance() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos % 1000) as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_error_codes() {
        assert_eq!(classify_error(402, ""), ErrorClass::Recoverable);
        assert_eq!(classify_error(403, ""), ErrorClass::Recoverable);
        assert_eq!(classify_error(429, ""), ErrorClass::Recoverable);
        assert_eq!(classify_error(400, "INVALID_MODEL_ID"), ErrorClass::Recoverable);
        assert_eq!(classify_error(400, "CONTENT_LENGTH_EXCEEDS"), ErrorClass::Fatal);
        assert_eq!(classify_error(400, "bad request"), ErrorClass::Fatal);
        assert_eq!(classify_error(422, ""), ErrorClass::Fatal);
        assert_eq!(classify_error(500, ""), ErrorClass::Fatal);
        assert_eq!(classify_error(503, ""), ErrorClass::Fatal);
    }

    #[test]
    fn circuit_breaker_exponential_backoff() {
        let mut cb = AccountCircuitBreaker::new();
        assert_eq!(cb.recovery_timeout(), Duration::from_secs(60));

        cb.record_failure();
        assert_eq!(cb.recovery_timeout(), Duration::from_secs(60));

        cb.record_failure();
        assert_eq!(cb.recovery_timeout(), Duration::from_secs(120));

        cb.record_failure();
        assert_eq!(cb.recovery_timeout(), Duration::from_secs(240));
    }

    #[test]
    fn circuit_breaker_reset_on_success() {
        let mut cb = AccountCircuitBreaker::new();
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.failures, 2);

        cb.record_success();
        assert_eq!(cb.failures, 0);
        assert_eq!(cb.state, CircuitState::Active);
    }

    #[test]
    fn error_class_recoverable_variants() {
        assert_eq!(classify_error(402, "quota"), ErrorClass::Recoverable);
        assert_eq!(classify_error(429, "rate limit"), ErrorClass::Recoverable);
    }

    #[test]
    fn load_balancing_mode_from_str() {
        assert_eq!(LoadBalancingMode::from_str("balanced"), LoadBalancingMode::Balanced);
        assert_eq!(LoadBalancingMode::from_str("priority"), LoadBalancingMode::Priority);
        assert_eq!(LoadBalancingMode::from_str("unknown"), LoadBalancingMode::Priority);
    }

    #[test]
    fn load_balancing_mode_as_str() {
        assert_eq!(LoadBalancingMode::Priority.as_str(), "priority");
        assert_eq!(LoadBalancingMode::Balanced.as_str(), "balanced");
    }
}
