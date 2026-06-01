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
    /// Classification of the last error (for self-healing decisions).
    last_error_class: Option<ErrorClass>,
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
            last_error_class: None,
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
    fn record_failure(&mut self, error_class: ErrorClass) {
        self.total_requests += 1;
        self.failed_requests += 1;
        self.failures += 1;
        self.last_failure = Some(Instant::now());
        self.last_error_class = Some(error_class);
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
    /// Number of currently inflight requests
    inflight_count: u32,
    /// Exponential moving average of response latency (ms)
    latency_ema: f64,
    /// Timestamp of last successful request
    last_success_at: Option<Instant>,
    /// Total recent requests for load balancing scoring
    recent_requests: u64,
}

/// Load balancing mode for account selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadBalancingMode {
    /// Sticky to current account, fall back on failure
    Priority,
    /// Round-robin across available accounts
    Balanced,
    /// Composite scoring: health + inflight + usage balance + idle time + latency + expiry
    Smart,
}

impl LoadBalancingMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "balanced" => Self::Balanced,
            "smart" => Self::Smart,
            _ => Self::Priority,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Balanced => "balanced",
            Self::Smart => "smart",
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
                    inflight_count: 0,
                    latency_ema: 0.0,
                    last_success_at: None,
                    recent_requests: 0,
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
            LoadBalancingMode::Smart => self.get_available_smart(exclude),
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

    /// Smart load balancing: composite scoring with jitter.
    fn get_available_smart(
        &mut self,
        exclude: &[String],
    ) -> Option<(&str, &Arc<Mutex<KiroAuthManager>>)> {
        // Collect available accounts with scores
        let avg_recent = {
            let total: u64 = self.accounts.iter().map(|a| a.recent_requests).sum();
            let count = self.accounts.len().max(1) as f64;
            total as f64 / count
        };

        let mut candidates: Vec<(usize, f64)> = Vec::new();
        for (i, account) in self.accounts.iter().enumerate() {
            if account.disabled || exclude.contains(&account.id) {
                continue;
            }
            if !account.circuit.is_available() {
                // Probabilistic retry for broken accounts
                if account.circuit.state == CircuitState::Broken && self.should_probabilistic_retry(i) {
                    let score = Self::compute_account_score(account, avg_recent);
                    candidates.push((i, score));
                }
                continue;
            }
            let score = Self::compute_account_score(account, avg_recent);
            candidates.push((i, score));
        }

        if candidates.is_empty() {
            return None;
        }

        // Sort by score descending (higher = better)
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let best_score = candidates[0].1;
        let threshold = (best_score.abs() * 0.15).max(5.0);

        // Select randomly from top candidates within threshold
        let top_candidates: Vec<usize> = candidates
            .iter()
            .filter(|(_, score)| (best_score - score).abs() <= threshold)
            .map(|(idx, _)| *idx)
            .collect();

        let selected_idx = if top_candidates.is_empty() {
            candidates[0].0
        } else {
            let pick = rand_index(top_candidates.len());
            top_candidates[pick]
        };

        self.current_index = selected_idx;
        let account = &self.accounts[selected_idx];
        Some((account.id.as_str(), &account.auth))
    }

    /// Compute composite score for smart load balancing. Higher = better.
    fn compute_account_score(account: &ManagedAccount, avg_recent: f64) -> f64 {
        let health = 100.0 - (account.circuit.failures as f64 * 10.0).min(100.0);
        let inflight_penalty = -(account.inflight_count as f64 * 30.0);
        let usage_balance = if avg_recent > 0.0 {
            let ratio = account.recent_requests as f64 / avg_recent;
            (-40.0f64).max(40.0 * (1.0 - ratio))
        } else if account.recent_requests == 0 {
            40.0
        } else {
            0.0
        };
        let zero_use_bonus = if account.recent_requests == 0 { 30.0 } else { 0.0 };
        let idle_bonus = account
            .last_success_at
            .map(|t| {
                let idle_secs = t.elapsed().as_secs_f64();
                if idle_secs > 30.0 {
                    (idle_secs / 60.0 * 5.0).min(20.0)
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        let latency_bonus = if account.latency_ema > 0.0 && account.latency_ema < 5000.0 {
            10.0
        } else {
            0.0
        };

        health + inflight_penalty + usage_balance + zero_use_bonus + idle_bonus + latency_bonus
    }

    /// Increment inflight count for the current account.
    pub fn increment_inflight(&mut self) {
        if let Some(account) = self.accounts.get_mut(self.current_index) {
            account.inflight_count += 1;
            account.recent_requests += 1;
        }
    }

    /// Decrement inflight count for the current account.
    pub fn release_inflight(&mut self) {
        if let Some(account) = self.accounts.get_mut(self.current_index) {
            account.inflight_count = account.inflight_count.saturating_sub(1);
        }
    }

    /// Record response latency with exponential moving average (alpha=0.3).
    pub fn record_response_latency(&mut self, latency_ms: f64) {
        if let Some(account) = self.accounts.get_mut(self.current_index) {
            if account.latency_ema == 0.0 {
                account.latency_ema = latency_ms;
            } else {
                account.latency_ema = account.latency_ema * 0.7 + latency_ms * 0.3;
            }
        }
    }

    /// Release inflight count and record response latency in one call.
    pub fn release_inflight_with_latency(&mut self, latency_ms: f64) {
        self.release_inflight();
        self.record_response_latency(latency_ms);
    }

    /// Record a successful request (also updates last_success_at).
    pub fn record_success(&mut self) {
        if let Some(account) = self.accounts.get_mut(self.current_index) {
            account.circuit.record_success();
            account.last_success_at = Some(Instant::now());
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

    /// Record a failed request for the current account.
    pub fn record_failure(&mut self, error_class: ErrorClass) {
        if let Some(account) = self.accounts.get_mut(self.current_index) {
            let id = account.id.clone();
            account.circuit.record_failure(error_class.clone());
            warn!(
                account_id = id.as_str(),
                failures = account.circuit.failures,
                backoff_secs = account.circuit.recovery_timeout().as_secs(),
                error_class = ?error_class,
                "账户 circuit breaker 触发"
            );
        }
    }

    /// Self-healing: when ALL non-disabled accounts are unavailable,
    /// halve error counts for accounts with recoverable errors,
    /// clear cooldowns, and move them to HalfOpen state.
    /// Returns true if any account was healed.
    pub fn self_heal(&mut self) -> bool {
        // Only trigger when all accounts are unavailable
        let all_unavailable = self.accounts.iter().all(|a| {
            a.disabled || !a.circuit.is_available()
        });
        if !all_unavailable {
            return false;
        }

        let mut healed = false;
        for account in &mut self.accounts {
            if account.disabled {
                continue;
            }
            // Only heal accounts with recoverable errors, not fatal/quota
            if account.circuit.state != CircuitState::Broken {
                continue;
            }
            match &account.circuit.last_error_class {
                Some(ErrorClass::Recoverable) | None => {
                    // Heal: halve error count, clear cooldown, move to HalfOpen
                    let old_failures = account.circuit.failures;
                    account.circuit.failures = account.circuit.failures / 2;
                    if account.circuit.failures == 0 {
                        account.circuit.state = CircuitState::Active;
                    } else {
                        account.circuit.state = CircuitState::HalfOpen;
                    }
                    account.circuit.last_failure = None;
                    healed = true;
                    info!(
                        account_id = account.id.as_str(),
                        old_failures,
                        new_failures = account.circuit.failures,
                        "自愈: 账户错误计数减半"
                    );
                }
                Some(ErrorClass::Fatal) => {
                    // Don't heal fatal errors (e.g., quota exhausted)
                }
            }
        }
        healed
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
            inflight_count: 0,
            latency_ema: 0.0,
            last_success_at: None,
            recent_requests: 0,
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

/// Simple pseudo-random index in range [0, n).
fn rand_index(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos as usize) % n
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

        cb.record_failure(ErrorClass::Recoverable);
        assert_eq!(cb.recovery_timeout(), Duration::from_secs(60));

        cb.record_failure(ErrorClass::Recoverable);
        assert_eq!(cb.recovery_timeout(), Duration::from_secs(120));

        cb.record_failure(ErrorClass::Recoverable);
        assert_eq!(cb.recovery_timeout(), Duration::from_secs(240));
    }

    #[test]
    fn circuit_breaker_reset_on_success() {
        let mut cb = AccountCircuitBreaker::new();
        cb.record_failure(ErrorClass::Recoverable);
        cb.record_failure(ErrorClass::Recoverable);
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

    #[test]
    fn self_heal_recovers_recoverable_errors() {
        let configs = vec![
            ("acc1".to_string(), make_test_config()),
            ("acc2".to_string(), make_test_config()),
        ];
        let client = reqwest::Client::new();
        let mut mgr = AccountManager::new(&configs, client);

        // Break both accounts with recoverable errors
        mgr.current_index = 0;
        mgr.record_failure(ErrorClass::Recoverable);
        mgr.record_failure(ErrorClass::Recoverable);
        mgr.record_failure(ErrorClass::Recoverable);
        mgr.current_index = 1;
        mgr.record_failure(ErrorClass::Recoverable);
        mgr.record_failure(ErrorClass::Recoverable);

        // Both should be unavailable now
        assert!(mgr.get_available_account(&[]).is_none());

        // Self-heal should recover them
        assert!(mgr.self_heal());

        // After healing, accounts should be available (failures halved, moved to HalfOpen)
        assert!(mgr.get_available_account(&[]).is_some());
    }

    #[test]
    fn self_heal_ignores_fatal_errors() {
        let configs = vec![
            ("acc1".to_string(), make_test_config()),
        ];
        let client = reqwest::Client::new();
        let mut mgr = AccountManager::new(&configs, client);

        // Break account with fatal error
        mgr.current_index = 0;
        mgr.record_failure(ErrorClass::Fatal);

        // Should be unavailable
        assert!(mgr.get_available_account(&[]).is_none());

        // Self-heal should NOT recover fatal errors
        assert!(!mgr.self_heal());
        assert!(mgr.get_available_account(&[]).is_none());
    }

    #[test]
    fn smart_load_balancing_selects_account() {
        let configs = vec![
            ("acc1".to_string(), make_test_config()),
            ("acc2".to_string(), make_test_config()),
        ];
        let client = reqwest::Client::new();
        let mut mgr = AccountManager::new_with_mode(
            &configs,
            client,
            LoadBalancingMode::Smart,
        );

        // Both accounts are fresh - smart mode should select one
        let result = mgr.get_available_account(&[]);
        assert!(result.is_some());
    }

    #[test]
    fn smart_mode_from_str() {
        assert_eq!(LoadBalancingMode::from_str("smart"), LoadBalancingMode::Smart);
        assert_eq!(LoadBalancingMode::Smart.as_str(), "smart");
    }

    fn make_test_config() -> KiroConfig {
        KiroConfig {
            auth_method: "social".to_string(),
            refresh_token: Some("test".to_string()),
            client_id: None,
            client_secret: None,
            profile_arn: None,
            region: "us-east-1".to_string(),
            api_region: None,
            model_aliases: None,
            hidden_models: None,
            kiro_version: None,
            proxy_url: None,
            thinking_mode: None,
            web_search_enabled: None,
            accounts: None,
            load_balancing_mode: None,
            agentic_prompt_injection: None,
        }
    }
}
