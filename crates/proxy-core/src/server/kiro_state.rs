//! KiroState: encapsulates all Kiro-specific runtime state.
//!
//! Extracted from AppState to reduce the "god object" pattern and keep
//! Kiro concerns isolated from generic proxy logic.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::info;

use crate::error::{AppError, Result};

use crate::config::{Config, KiroConfig, ProviderFormat};
use crate::convert::kiro::account::{AccountManager, LoadBalancingMode};
use crate::convert::kiro::auth::KiroAuthManager;
use crate::convert::kiro::endpoint_health::EndpointHealthTracker;
use crate::convert::kiro::flow_monitor::FlowMonitor;
use crate::convert::kiro::rate_limiter::RateLimiter;
use crate::convert::kiro::smart_summary::SummaryCache;
use crate::convert::kiro::truncation::TruncationState;

/// All Kiro-specific runtime state, only initialized when a Kiro provider exists.
#[derive(Clone)]
pub struct KiroState {
    /// Provider name this runtime state was initialized from.
    pub provider_name: String,
    /// Single-account auth manager (mutually exclusive with `account_manager`).
    pub auth: Option<Arc<Mutex<KiroAuthManager>>>,
    /// Multi-account manager for load balancing / failover.
    pub account_manager: Option<Arc<Mutex<AccountManager>>>,
    /// Request/response flow tracking.
    pub flow_monitor: Arc<Mutex<FlowMonitor>>,
    /// Per-account + global RPM rate limiter.
    pub rate_limiter: Arc<Mutex<RateLimiter>>,
    /// Endpoint health scoring for multi-endpoint fallback.
    pub endpoint_health: EndpointHealthTracker,
    /// Truncation recovery state — stores truncation info between requests.
    pub truncation_state: TruncationState,
    /// LLM Smart Summary cache for CONTENT_TOO_LONG retry.
    pub summary_cache: Arc<SummaryCache>,
    /// Cached /v1/models response (last_update, response_json).
    pub model_cache: Arc<Mutex<(Instant, serde_json::Value)>>,
    /// Default upstream client (no proxy), reused across requests for connection pooling.
    default_client: reqwest::Client,
    /// Cache of proxied HTTP clients keyed by proxy URL, to avoid rebuilding a
    /// client (and discarding the connection pool) on every request.
    proxied_clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
}

impl KiroState {
    /// Initialize KiroState from config and HTTP client.
    ///
    /// Returns `None` if no Kiro provider is configured.
    pub fn from_config(config: &Config, client: &reqwest::Client) -> Option<Self> {
        let active_kiro = config
            .active_provider_config()
            .ok()
            .filter(|p| p.format == ProviderFormat::Kiro);
        let kiro_provider = active_kiro.or_else(|| {
            config
                .providers
                .iter()
                .find(|p| p.format == ProviderFormat::Kiro)
        })?;

        let kiro_config = kiro_provider.kiro_config.as_ref()?;

        let (auth, account_manager) = init_kiro_auth(kiro_config, &kiro_provider.name, client);

        Some(Self {
            provider_name: kiro_provider.name.clone(),
            auth,
            account_manager,
            flow_monitor: Arc::new(Mutex::new(FlowMonitor::new(1000))),
            rate_limiter: Arc::new(Mutex::new(RateLimiter::new(0, 0, 0))),
            endpoint_health: EndpointHealthTracker::new(),
            truncation_state: TruncationState::new(),
            summary_cache: Arc::new(SummaryCache::new()),
            model_cache: Arc::new(Mutex::new((Instant::now(), serde_json::Value::Null))),
            default_client: client.clone(),
            proxied_clients: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Get an HTTP client suitable for the given optional proxy URL.
    ///
    /// - When no proxy is requested, returns the shared default client.
    /// - When a proxy is requested, returns a cached client for that proxy URL,
    ///   building and caching one on first use. This preserves connection
    ///   pooling instead of rebuilding a client on every request.
    ///
    /// Returns an error if a proxy URL is configured but the client cannot be
    /// built (e.g. malformed proxy URL), rather than silently falling back to a
    /// direct connection.
    pub async fn client_for_proxy(&self, proxy_url: Option<&str>) -> Result<reqwest::Client> {
        let Some(proxy) = proxy_url else {
            return Ok(self.default_client.clone());
        };

        {
            let cache = self.proxied_clients.lock().await;
            if let Some(client) = cache.get(proxy) {
                return Ok(client.clone());
            }
        }

        let client = crate::server::http_client::build_proxied_client(Some(proxy)).map_err(|e| {
            AppError::Config(format!("构建代理 HTTP 客户端失败 (proxy_url={}): {}", proxy, e))
        })?;

        let mut cache = self.proxied_clients.lock().await;
        // Another task may have inserted concurrently; keep the first one.
        let entry = cache.entry(proxy.to_string()).or_insert(client);
        Ok(entry.clone())
    }

    /// Start background scheduler for token pre-refresh.
    /// Pre-refreshes tokens when they are expiring soon.
    pub fn start_background_scheduler(&self) {
        if let Some(ref auth_arc) = self.auth {
            let auth = auth_arc.clone();
            let interval = std::time::Duration::from_secs(600); // 10 minutes
            tokio::spawn(async move {
                let mut timer = tokio::time::interval(interval);
                loop {
                    timer.tick().await;
                    let needs_refresh = {
                        let auth_guard = auth.lock().await;
                        auth_guard
                            .credentials_iter()
                            .filter(|c| c.is_expiring_soon())
                            .count()
                            > 0
                    };
                    if needs_refresh {
                        info!("后台调度器: 预刷新 Kiro token");
                        let mut auth_guard = auth.lock().await;
                        if let Err(e) = auth_guard.get_valid_token().await {
                            tracing::warn!(error = %e, "后台 token 预刷新失败");
                        }
                    }
                }
            });
        }
    }
}

/// Initialize Kiro auth managers from config.
///
/// If multiple accounts are configured, creates an AccountManager.
/// Otherwise creates a single KiroAuthManager.
fn init_kiro_auth(
    kiro_config: &KiroConfig,
    provider_name: &str,
    client: &reqwest::Client,
) -> (
    Option<Arc<Mutex<KiroAuthManager>>>,
    Option<Arc<Mutex<AccountManager>>>,
) {
    let accounts = collect_kiro_accounts(kiro_config, provider_name);

    if accounts.len() > 1 {
        let mode = kiro_config
            .load_balancing_mode
            .as_deref()
            .map(LoadBalancingMode::from_str)
            .unwrap_or(LoadBalancingMode::Priority);
        let mode_str = mode.as_str().to_string();
        let mgr = AccountManager::new_with_mode(&accounts, client.clone(), mode);
        info!(
            account_count = accounts.len(),
            mode = mode_str.as_str(),
            "初始化 Kiro 多账户管理器"
        );
        let mgr_arc = Arc::new(Mutex::new(mgr));
        AccountManager::start_periodic_save(mgr_arc.clone());
        (None, Some(mgr_arc))
    } else if accounts.len() == 1 {
        let auth = KiroAuthManager::new(&accounts[0].1, client.clone());
        (Some(Arc::new(Mutex::new(auth))), None)
    } else {
        // No accounts, create from flat fields (backward compat)
        let auth = KiroAuthManager::new(kiro_config, client.clone());
        (Some(Arc::new(Mutex::new(auth))), None)
    }
}

/// Collect all Kiro account entries from config.
///
/// If `accounts` is populated, expands each entry into a full KiroConfig.
/// Otherwise returns an empty vec (caller uses the flat config directly).
fn collect_kiro_accounts(kiro_config: &KiroConfig, provider_name: &str) -> Vec<(String, KiroConfig)> {
    let Some(ref accounts) = kiro_config.accounts else {
        return vec![];
    };
    if accounts.is_empty() {
        return vec![];
    }

    accounts
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let id = format!("{}:{}", provider_name, i);
            let cfg = KiroConfig {
                auth_method: entry
                    .auth_method
                    .clone()
                    .unwrap_or_else(|| kiro_config.auth_method.clone()),
                refresh_token: entry.refresh_token.clone(),
                client_id: entry.client_id.clone().or_else(|| kiro_config.client_id.clone()),
                client_secret: entry
                    .client_secret
                    .clone()
                    .or_else(|| kiro_config.client_secret.clone()),
                profile_arn: entry
                    .profile_arn
                    .clone()
                    .or_else(|| kiro_config.profile_arn.clone()),
                region: entry
                    .region
                    .clone()
                    .unwrap_or_else(|| kiro_config.region.clone()),
                api_region: entry
                    .api_region
                    .clone()
                    .or_else(|| kiro_config.api_region.clone()),
                model_aliases: kiro_config.model_aliases.clone(),
                hidden_models: kiro_config.hidden_models.clone(),
                kiro_version: kiro_config.kiro_version.clone(),
                proxy_url: entry.proxy_url.clone().or_else(|| kiro_config.proxy_url.clone()),
                thinking_mode: kiro_config.thinking_mode.clone(),
                web_search_enabled: kiro_config.web_search_enabled,
                accounts: None,
                load_balancing_mode: None,
                agentic_prompt_injection: kiro_config.agentic_prompt_injection,
                first_token_timeout: kiro_config.first_token_timeout,
                streaming_read_timeout: kiro_config.streaming_read_timeout,
                first_token_max_retries: kiro_config.first_token_max_retries,
                quota_cooldown_secs: kiro_config.quota_cooldown_secs,
                health_score_decay: kiro_config.health_score_decay,
                health_score_recovery: kiro_config.health_score_recovery,
                preferred_endpoint: kiro_config.preferred_endpoint.clone(),
                endpoint_fallback: kiro_config.endpoint_fallback,
                debug_save_requests: kiro_config.debug_save_requests,
                smart_summary_enabled: kiro_config.smart_summary_enabled,
                enable_quota_check: kiro_config.enable_quota_check,
                quota_check_interval_secs: kiro_config.quota_check_interval_secs,
            };
            (id, cfg)
        })
        .collect()
}
