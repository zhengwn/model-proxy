//! Rate limiter for Kiro API requests.
//!
//! Enforces per-account and global RPM (requests per minute) limits
//! with configurable minimum request intervals.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Rate limiter with per-account and global limits.
pub struct RateLimiter {
    /// Minimum interval between requests to any account
    min_interval: Duration,
    /// Per-account RPM limit (0 = unlimited)
    per_account_rpm: u32,
    /// Global RPM limit (0 = unlimited)
    global_rpm: u32,
    /// Per-account request timestamps (sliding window)
    account_requests: HashMap<String, Vec<Instant>>,
    /// Global request timestamps
    global_requests: Vec<Instant>,
    /// Last request time (for min_interval enforcement)
    last_request: Option<Instant>,
}

impl RateLimiter {
    pub fn new(min_interval_ms: u64, per_account_rpm: u32, global_rpm: u32) -> Self {
        Self {
            min_interval: Duration::from_millis(min_interval_ms),
            per_account_rpm,
            global_rpm,
            account_requests: HashMap::new(),
            global_requests: Vec::new(),
            last_request: None,
        }
    }

    /// Check if a request is allowed for the given account.
    /// Returns Ok(wait_duration) if allowed (wait_duration may be > 0 if we need to throttle),
    /// or Err(wait_duration) if rate limited.
    pub fn check(&mut self, account_id: &str) -> Result<Duration, Duration> {
        let now = Instant::now();

        // Enforce minimum interval
        if let Some(last) = self.last_request {
            let elapsed = now.duration_since(last);
            if elapsed < self.min_interval {
                let wait = self.min_interval - elapsed;
                return Err(wait);
            }
        }

        // Clean old entries (older than 60 seconds)
        let cutoff = now - Duration::from_secs(60);
        self.global_requests.retain(|t| *t > cutoff);
        if let Some(reqs) = self.account_requests.get_mut(account_id) {
            reqs.retain(|t| *t > cutoff);
        }

        // Check per-account RPM
        if self.per_account_rpm > 0 {
            let account_reqs = self.account_requests.get(account_id).map(|v| v.len()).unwrap_or(0);
            if account_reqs >= self.per_account_rpm as usize {
                let oldest = self.account_requests
                    .get(account_id)
                    .and_then(|v| v.first())
                    .map(|t| Duration::from_secs(60).saturating_sub(now.duration_since(*t)))
                    .unwrap_or(Duration::from_secs(1));
                warn!(
                    account_id,
                    rpm = self.per_account_rpm,
                    "账户 RPM 限制"
                );
                return Err(oldest);
            }
        }

        // Check global RPM
        if self.global_rpm > 0 && self.global_requests.len() >= self.global_rpm as usize {
            let oldest = self.global_requests
                .first()
                .map(|t| Duration::from_secs(60).saturating_sub(now.duration_since(*t)))
                .unwrap_or(Duration::from_secs(1));
            warn!(rpm = self.global_rpm, "全局 RPM 限制");
            return Err(oldest);
        }

        // Record the request
        self.last_request = Some(now);
        self.global_requests.push(now);
        self.account_requests
            .entry(account_id.to_string())
            .or_default()
            .push(now);

        Ok(Duration::ZERO)
    }

    /// Wait until a request is allowed, then proceed.
    /// Returns the wait duration that was slept (if any).
    /// Note: callers should NOT hold the Mutex guard across this call.
    /// Instead, use `check()` to get the wait duration, drop the guard,
    /// sleep, then re-acquire and call `check()` again.
    pub fn wait_duration(&mut self, account_id: &str) -> Duration {
        match self.check(account_id) {
            Ok(_) => Duration::ZERO,
            Err(wait) => wait,
        }
    }
}

/// Shared rate limiter type.
pub type SharedRateLimiter = Arc<Mutex<RateLimiter>>;

/// Quota information from getUsageLimits API.
#[derive(Debug, Clone)]
pub struct QuotaInfo {
    pub remaining: f64,
    pub limit: f64,
    pub last_checked: Instant,
    pub days_until_reset: Option<u32>,
}

impl QuotaInfo {
    pub fn is_exhausted(&self) -> bool {
        self.remaining <= 0.0
    }

    pub fn is_stale(&self, interval: Duration) -> bool {
        self.last_checked.elapsed() > interval
    }
}

/// Call getUsageLimits API to check account quota.
/// Returns QuotaInfo on success. On failure, logs warning and returns None.
pub async fn check_quota(
    client: &reqwest::Client,
    access_token: &str,
    api_region: &str,
) -> Option<QuotaInfo> {
    let url = format!("https://q.{}.amazonaws.com/getUsageLimits", api_region);
    match client
        .post(&url)
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("Authorization", format!("Bearer {}", access_token))
        .header(
            "x-amz-target",
            "AmazonCodeWhispererStreamingService.GetUsageLimits",
        )
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(json) => parse_quota_response(&json),
            Err(e) => {
                warn!(error = %e, "解析 getUsageLimits 响应失败");
                None
            }
        },
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            warn!(status, body = body.as_str(), "getUsageLimits 请求失败");
            None
        }
        Err(e) => {
            warn!(error = %e, "getUsageLimits 网络错误");
            None
        }
    }
}

fn parse_quota_response(json: &serde_json::Value) -> Option<QuotaInfo> {
    let breakdown = json.get("usageBreakdownList").and_then(|v| v.as_array());
    let entry = breakdown
        .and_then(|arr| arr.first())
        .or_else(|| json.as_object().map(|_| json));

    let entry = entry?;

    let limit = entry
        .get("usageLimitWithPrecision")
        .and_then(|v| v.as_f64())
        .or_else(|| entry.get("usageLimit").and_then(|v| v.as_f64()))
        .unwrap_or(f64::MAX);

    let used = entry
        .get("currentUsageWithPrecision")
        .and_then(|v| v.as_f64())
        .or_else(|| entry.get("currentUsage").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);

    let remaining = (limit - used).max(0.0);

    let days_until_reset = json
        .get("daysUntilReset")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    Some(QuotaInfo {
        remaining,
        limit,
        last_checked: Instant::now(),
        days_until_reset,
    })
}

/// Quota cooldown manager for rate-limited accounts.
pub struct QuotaManager {
    /// Account ID -> cooldown expiry time
    cooldowns: HashMap<String, Instant>,
    /// Default cooldown duration
    default_cooldown: Duration,
    /// Per-account quota info from getUsageLimits
    quota_data: HashMap<String, QuotaInfo>,
    /// Quota check interval
    check_interval: Duration,
    /// Whether proactive quota checking is enabled
    quota_check_enabled: bool,
}

impl QuotaManager {
    pub fn new(default_cooldown_secs: u64) -> Self {
        Self {
            cooldowns: HashMap::new(),
            default_cooldown: Duration::from_secs(default_cooldown_secs),
            quota_data: HashMap::new(),
            check_interval: Duration::from_secs(600),
            quota_check_enabled: false,
        }
    }

    pub fn with_quota_check(mut self, enabled: bool, interval_secs: u64) -> Self {
        self.quota_check_enabled = enabled;
        self.check_interval = Duration::from_secs(interval_secs);
        self
    }

    /// Check if an account is in cooldown.
    pub fn is_in_cooldown(&self, account_id: &str) -> bool {
        if let Some(expiry) = self.cooldowns.get(account_id) {
            Instant::now() < *expiry
        } else {
            false
        }
    }

    /// Get remaining cooldown time.
    pub fn remaining_cooldown(&self, account_id: &str) -> Option<Duration> {
        self.cooldowns.get(account_id).and_then(|expiry| {
            let now = Instant::now();
            if now < *expiry {
                Some(*expiry - now)
            } else {
                None
            }
        })
    }

    /// Put an account in cooldown.
    pub fn set_cooldown(&mut self, account_id: &str) {
        self.cooldowns.insert(
            account_id.to_string(),
            Instant::now() + self.default_cooldown,
        );
    }

    /// Put an account in cooldown with custom duration.
    pub fn set_cooldown_duration(&mut self, account_id: &str, duration: Duration) {
        self.cooldowns.insert(
            account_id.to_string(),
            Instant::now() + duration,
        );
    }

    /// Remove an account from cooldown (manual restore).
    pub fn clear_cooldown(&mut self, account_id: &str) {
        self.cooldowns.remove(account_id);
    }

    /// Get all accounts in cooldown.
    pub fn accounts_in_cooldown(&self) -> Vec<&str> {
        let now = Instant::now();
        self.cooldowns
            .iter()
            .filter(|(_, expiry)| now < **expiry)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Check if quota is exhausted for an account (proactive check).
    pub fn is_quota_exhausted(&self, account_id: &str) -> bool {
        if !self.quota_check_enabled {
            return false;
        }
        self.quota_data
            .get(account_id)
            .map(|q| q.is_exhausted())
            .unwrap_or(false)
    }

    /// Check if quota data is stale for an account.
    pub fn is_quota_stale(&self, account_id: &str) -> bool {
        self.quota_data
            .get(account_id)
            .map(|q| q.is_stale(self.check_interval))
            .unwrap_or(true)
    }

    /// Update quota info for an account. Auto-sets cooldown if exhausted.
    pub fn set_quota_info(&mut self, account_id: &str, info: QuotaInfo) {
        if info.is_exhausted() {
            self.set_cooldown(account_id);
            warn!(account_id, "配额耗尽，自动进入冷却");
        }
        debug!(
            account_id,
            remaining = info.remaining,
            limit = info.limit,
            "配额信息已更新"
        );
        self.quota_data.insert(account_id.to_string(), info);
    }

    /// Get quota info for an account.
    pub fn get_quota_info(&self, account_id: &str) -> Option<&QuotaInfo> {
        self.quota_data.get(account_id)
    }

    /// Whether quota checking is enabled.
    pub fn quota_check_enabled(&self) -> bool {
        self.quota_check_enabled
    }

    /// Get the check interval.
    pub fn check_interval(&self) -> Duration {
        self.check_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_basic() {
        let mut limiter = RateLimiter::new(0, 0, 0); // no limits
        assert!(limiter.check("account1").is_ok());
        assert!(limiter.check("account1").is_ok());
    }

    #[test]
    fn rate_limiter_per_account_rpm() {
        let mut limiter = RateLimiter::new(0, 2, 0); // 2 RPM per account
        assert!(limiter.check("acc1").is_ok());
        assert!(limiter.check("acc1").is_ok());
        assert!(limiter.check("acc1").is_err()); // 3rd request within 60s
        assert!(limiter.check("acc2").is_ok()); // different account
    }

    #[test]
    fn rate_limiter_global_rpm() {
        let mut limiter = RateLimiter::new(0, 0, 2); // 2 RPM global
        assert!(limiter.check("acc1").is_ok());
        assert!(limiter.check("acc2").is_ok());
        assert!(limiter.check("acc3").is_err()); // global limit
    }

    #[test]
    fn quota_manager_cooldown() {
        let mut qm = QuotaManager::new(300);
        assert!(!qm.is_in_cooldown("acc1"));

        qm.set_cooldown("acc1");
        assert!(qm.is_in_cooldown("acc1"));
        assert!(qm.remaining_cooldown("acc1").is_some());

        qm.clear_cooldown("acc1");
        assert!(!qm.is_in_cooldown("acc1"));
    }

    #[test]
    fn quota_manager_multiple_accounts() {
        let mut qm = QuotaManager::new(300);
        qm.set_cooldown("acc1");
        qm.set_cooldown("acc2");

        let cooldown = qm.accounts_in_cooldown();
        assert_eq!(cooldown.len(), 2);
    }
}
