use arc_swap::ArcSwap;
use reqwest::Client;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use tracing::info;

use crate::config::{Config, ConfigError, KiroConfig, ModelRoute, ProviderConfig, ProviderFormat};
use crate::logging::LogCollector;
use crate::provider_registry::ProviderRegistry;
use crate::server::ip_filter::IpFilter;
use crate::server::site_guard::SiteGuardConfig;
use crate::RequestCounters;

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
    pub active_provider: Arc<ArcSwap<ProviderConfig>>,
    pub registry: Arc<ArcSwap<ProviderRegistry>>,
    pub model_routes: Arc<ArcSwap<Vec<ModelRoute>>>,
    pub client: Client,
    pub log_collector: Arc<LogCollector>,
    /// Optional request counters for tracking total/failed requests.
    pub counters: Option<RequestCounters>,
    /// Optional semaphore for concurrency limiting.
    pub concurrency_semaphore: Option<Arc<Semaphore>>,
    /// Shared Kiro auth manager (only initialized when a Kiro provider exists).
    pub kiro_auth: Option<Arc<tokio::sync::Mutex<crate::convert::kiro::auth::KiroAuthManager>>>,
    /// Multi-account manager for Kiro failover (when multiple Kiro configs exist).
    pub kiro_account_manager: Option<Arc<tokio::sync::Mutex<crate::convert::kiro::account::AccountManager>>>,
    /// Flow monitor for request/response tracking.
    pub flow_monitor: Option<Arc<tokio::sync::Mutex<crate::convert::kiro::flow_monitor::FlowMonitor>>>,
    /// Rate limiter for Kiro API requests.
    pub rate_limiter: Option<Arc<tokio::sync::Mutex<crate::convert::kiro::rate_limiter::RateLimiter>>>,
    /// Cached /v1/models response (Instant = last update time, Value = JSON response).
    pub model_cache: Option<Arc<tokio::sync::Mutex<(Instant, serde_json::Value)>>>,
    /// IP blacklist and request-rate tracker.
    pub ip_filter: IpFilter,
    /// SiteGuard config: maintenance mode and self-use mode toggles.
    pub site_guard: SiteGuardConfig,
    /// Prometheus-style metrics collector.
    pub metrics: Option<Arc<crate::server::metrics::Metrics>>,
}

/// Collect Kiro accounts from config and initialize auth managers.
///
/// If a Kiro provider has `accounts` with multiple entries, creates an AccountManager.
/// Otherwise creates a single KiroAuthManager for backward compatibility.
fn init_kiro_auth(
    config: &Config,
    client: &Client,
) -> (
    Option<Arc<tokio::sync::Mutex<crate::convert::kiro::auth::KiroAuthManager>>>,
    Option<Arc<tokio::sync::Mutex<crate::convert::kiro::account::AccountManager>>>,
) {
    use crate::convert::kiro::account::{AccountManager, LoadBalancingMode};
    use crate::convert::kiro::auth::KiroAuthManager;

    // Find the first Kiro provider
    let kiro_provider = match config.providers.iter().find(|p| p.format == ProviderFormat::Kiro) {
        Some(p) => p,
        None => return (None, None),
    };

    let kiro_config = match kiro_provider.kiro_config.as_ref() {
        Some(c) => c,
        None => return (None, None),
    };

    // Collect all account entries
    let accounts: Vec<(String, KiroConfig)> =
        collect_kiro_accounts(kiro_config, &kiro_provider.name);

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
        (None, Some(Arc::new(tokio::sync::Mutex::new(mgr))))
    } else if accounts.len() == 1 {
        let auth = KiroAuthManager::new(&accounts[0].1, client.clone());
        (Some(Arc::new(tokio::sync::Mutex::new(auth))), None)
    } else {
        // No accounts, create from flat fields (backward compat)
        let auth = KiroAuthManager::new(kiro_config, client.clone());
        (Some(Arc::new(tokio::sync::Mutex::new(auth))), None)
    }
}

/// Collect all Kiro account entries from config.
///
/// If `accounts` is populated, expands each entry into a full KiroConfig.
/// Otherwise, falls back to the flat top-level fields (single account).
fn collect_kiro_accounts(kiro_config: &KiroConfig, provider_name: &str) -> Vec<(String, KiroConfig)> {
    if let Some(ref accounts) = kiro_config.accounts {
        if accounts.is_empty() {
            return vec![];
        }
        return accounts
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
                    profile_arn: entry.profile_arn.clone().or_else(|| kiro_config.profile_arn.clone()),
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
                };
                (id, cfg)
            })
            .collect();
    }
    vec![]
}

impl AppState {
    /// Create a new AppState from a Config (standalone mode, e.g. CLI).
    pub fn new(config: Config) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(UPSTREAM_CONNECT_TIMEOUT_SECS))
            .pool_idle_timeout(Duration::from_secs(UPSTREAM_POOL_IDLE_TIMEOUT_SECS))
            .pool_max_idle_per_host(UPSTREAM_POOL_MAX_IDLE_PER_HOST)
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .expect("构建 HTTP 客户端失败");

        // Build the provider registry from config.providers
        let registry =
            ProviderRegistry::new(config.providers.clone()).expect("构建 ProviderRegistry 失败");

        // Get the active provider config
        let active = config
            .active_provider_config()
            .expect("获取活跃 Provider 失败");

        let active_provider = Arc::new(ArcSwap::from_pointee(active.clone()));
        let model_routes = Arc::new(ArcSwap::from_pointee(config.model_routes.clone()));

        // Create a default LogCollector from the config's logging section
        let log_config = Arc::new(ArcSwap::from_pointee(config.logging.clone()));
        let log_collector = Arc::new(LogCollector::new(log_config, 256));

        let concurrency_semaphore = if config.server.max_concurrent_requests > 0 {
            Some(Arc::new(Semaphore::new(
                config.server.max_concurrent_requests,
            )))
        } else {
            None
        };

        // Initialize Kiro auth: multi-account manager or single auth manager
        let (kiro_auth, kiro_account_manager) =
            init_kiro_auth(&config, &client);

        Self {
            config: Arc::new(config),
            active_provider,
            registry: Arc::new(ArcSwap::from_pointee(registry)),
            model_routes,
            client,
            log_collector,
            counters: None,
            concurrency_semaphore,
            kiro_auth,
            kiro_account_manager,
            flow_monitor: Some(Arc::new(tokio::sync::Mutex::new(
                crate::convert::kiro::flow_monitor::FlowMonitor::new(1000),
            ))),
            rate_limiter: Some(Arc::new(tokio::sync::Mutex::new(
                crate::convert::kiro::rate_limiter::RateLimiter::new(0, 0, 0),
            ))),
            model_cache: None,
            ip_filter: IpFilter::new(),
            site_guard: SiteGuardConfig::default(),
            metrics: Some(Arc::new(crate::server::metrics::Metrics::new())),
        }
    }

    /// Create a new AppState with shared active_provider and registry (Tauri mode).
    /// This allows the Tauri layer and proxy-core to share the same ArcSwap instances.
    pub fn new_shared(
        config: Config,
        active_provider: Arc<ArcSwap<ProviderConfig>>,
        registry: Arc<ArcSwap<ProviderRegistry>>,
        model_routes: Arc<ArcSwap<Vec<ModelRoute>>>,
        log_collector: Arc<LogCollector>,
    ) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(UPSTREAM_CONNECT_TIMEOUT_SECS))
            .pool_idle_timeout(Duration::from_secs(UPSTREAM_POOL_IDLE_TIMEOUT_SECS))
            .pool_max_idle_per_host(UPSTREAM_POOL_MAX_IDLE_PER_HOST)
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .expect("构建 HTTP 客户端失败");

        let concurrency_semaphore = if config.server.max_concurrent_requests > 0 {
            Some(Arc::new(Semaphore::new(
                config.server.max_concurrent_requests,
            )))
        } else {
            None
        };

        // Initialize Kiro auth: multi-account manager or single auth manager
        let (kiro_auth, kiro_account_manager) =
            init_kiro_auth(&config, &client);

        Self {
            config: Arc::new(config),
            active_provider,
            registry,
            model_routes,
            client,
            log_collector,
            counters: None,
            concurrency_semaphore,
            kiro_auth,
            kiro_account_manager,
            flow_monitor: Some(Arc::new(tokio::sync::Mutex::new(
                crate::convert::kiro::flow_monitor::FlowMonitor::new(1000),
            ))),
            rate_limiter: Some(Arc::new(tokio::sync::Mutex::new(
                crate::convert::kiro::rate_limiter::RateLimiter::new(0, 0, 0),
            ))),
            model_cache: None,
            ip_filter: IpFilter::new(),
            site_guard: SiteGuardConfig::default(),
            metrics: Some(Arc::new(crate::server::metrics::Metrics::new())),
        }
    }

    /// Set the request counters for tracking total/failed requests.
    /// Must be called before the server starts accepting requests.
    pub fn set_counters(&mut self, counters: RequestCounters) {
        self.counters = Some(counters);
    }

    /// Increment the total request counter.
    pub fn inc_total_requests(&self) {
        if let Some(ref c) = self.counters {
            c.total.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Increment the failed request counter.
    pub fn inc_failed_requests(&self) {
        if let Some(ref c) = self.counters {
            c.failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 获取当前活跃 Provider（无锁）
    pub fn current_provider(&self) -> arc_swap::Guard<Arc<ProviderConfig>> {
        self.active_provider.load()
    }

    /// 获取当前模型路由（无锁）
    pub fn current_model_routes(&self) -> arc_swap::Guard<Arc<Vec<ModelRoute>>> {
        self.model_routes.load()
    }

    /// 切换活跃 Provider
    pub fn switch_provider(&self, name: &str) -> Result<(), ConfigError> {
        let registry = self.registry.load();
        let provider = registry
            .get(name)
            .ok_or_else(|| ConfigError::ProviderNotFound(name.to_string()))?;

        self.active_provider.store(Arc::new(provider.clone()));
        Ok(())
    }

    /// Start background scheduler for token pre-refresh and health checks.
    /// Pre-refreshes tokens 15 minutes before expiry.
    /// Runs health checks every 10 minutes.
    pub fn start_background_scheduler(&self) {
        if let Some(ref auth_arc) = self.kiro_auth {
            let auth = auth_arc.clone();
            let interval = Duration::from_secs(600); // 10 minutes
            tokio::spawn(async move {
                let mut timer = tokio::time::interval(interval);
                loop {
                    timer.tick().await;
                    // Pre-refresh: check if token needs refresh
                    let needs_refresh = {
                        let auth_guard = auth.lock().await;
                        let count = auth_guard.credentials_iter().filter(|c| c.is_expiring_soon()).count();
                        count > 0
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
