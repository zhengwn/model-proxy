use arc_swap::ArcSwap;
use reqwest::Client;
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::Arc,
    time::Instant,
};
use tokio::sync::Semaphore;

use crate::config::{Config, ConfigError, ModelRoute, ProviderConfig};
use crate::logging::LogCollector;
use crate::provider_registry::ProviderRegistry;
use crate::server::http_client::build_upstream_client;
use crate::server::ip_filter::IpFilter;
use crate::server::kiro_state::KiroState;
use crate::server::site_guard::SiteGuardConfig;
use crate::RequestCounters;

pub(crate) const ANTHROPIC_BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";
pub(crate) const MAX_LOG_BODY_BYTES: usize = 4096;
pub(crate) const NON_STREAM_REQUEST_TIMEOUT_SECS: u64 = 300;
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

/// Get current epoch time in seconds (shared utility).
pub(crate) fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) struct RequestCompletionGuard {
    request_id: String,
    request_start: Instant,
    phase: &'static str,
    completed: bool,
    metrics: Option<Arc<super::metrics::Metrics>>,
}

impl RequestCompletionGuard {
    pub(crate) fn new(request_id: String, request_start: Instant) -> Self {
        Self {
            request_id,
            request_start,
            phase: "received",
            completed: false,
            metrics: None,
        }
    }

    pub(crate) fn set_phase(&mut self, phase: &'static str) {
        self.phase = phase;
    }

    pub(crate) fn set_metrics(&mut self, metrics: Arc<super::metrics::Metrics>) {
        self.metrics = Some(metrics);
    }

    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RequestCompletionGuard {
    fn drop(&mut self) {
        // Decrement active connections counter
        if let Some(ref metrics) = self.metrics {
            metrics.connection_end();
        }
        if !self.completed {
            tracing::info!(
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
    /// Whether model routing is enabled (can be toggled at runtime).
    pub model_routes_enabled: Arc<AtomicBool>,
    pub client: Client,
    pub log_collector: Arc<LogCollector>,
    /// Optional request counters for tracking total/failed requests.
    pub counters: Option<RequestCounters>,
    /// Optional semaphore for concurrency limiting.
    pub concurrency_semaphore: Option<Arc<Semaphore>>,
    /// Kiro-specific state (auth, accounts, rate limiter, etc.).
    /// Only initialized when a Kiro provider exists in config.
    pub kiro: Option<KiroState>,
    /// IP blacklist and request-rate tracker.
    pub ip_filter: IpFilter,
    /// SiteGuard config: maintenance mode and self-use mode toggles.
    pub site_guard: SiteGuardConfig,
    /// Prometheus-style metrics collector.
    pub metrics: Option<Arc<crate::server::metrics::Metrics>>,
}

/// Parts of `AppState` whose construction is identical regardless of
/// whether the state is built standalone (CLI) or shared (Tauri).
/// Factored out so `AppState::new` and `AppState::new_shared` don't
/// duplicate this logic.
struct CommonStateParts {
    concurrency_semaphore: Option<Arc<Semaphore>>,
    kiro: Option<KiroState>,
    ip_filter: IpFilter,
    site_guard: SiteGuardConfig,
    metrics: Option<Arc<super::metrics::Metrics>>,
}

impl CommonStateParts {
    fn build(config: &Config, client: &Client) -> Self {
        let concurrency_semaphore = if config.server.max_concurrent_requests > 0 {
            Some(Arc::new(Semaphore::new(
                config.server.max_concurrent_requests,
            )))
        } else {
            None
        };

        // Initialize Kiro state (returns None if no Kiro provider configured)
        let kiro = KiroState::from_config(config, client);

        Self {
            concurrency_semaphore,
            kiro,
            ip_filter: IpFilter::new(),
            site_guard: SiteGuardConfig::default(),
            metrics: Some(Arc::new(crate::server::metrics::Metrics::new())),
        }
    }
}

impl AppState {
    /// Create a new AppState from a Config (standalone mode, e.g. CLI).
    pub fn new(config: Config) -> Self {
        let client = build_upstream_client();

        // Build the provider registry from config.providers
        let registry =
            ProviderRegistry::new(config.providers.clone()).expect("构建 ProviderRegistry 失败");

        // Get the active provider config
        let active = config
            .active_provider_config()
            .expect("获取活跃 Provider 失败");

        let active_provider = Arc::new(ArcSwap::from_pointee(active.clone()));
        let model_routes = Arc::new(ArcSwap::from_pointee(config.model_routes.clone()));
        let model_routes_enabled = Arc::new(AtomicBool::new(config.model_routes_enabled));

        // Create a default LogCollector from the config's logging section
        let log_config = Arc::new(ArcSwap::from_pointee(config.logging.clone()));
        let log_collector = Arc::new(LogCollector::new(log_config, 256));

        let common = CommonStateParts::build(&config, &client);

        Self {
            config: Arc::new(config),
            active_provider,
            registry: Arc::new(ArcSwap::from_pointee(registry)),
            model_routes,
            model_routes_enabled,
            client,
            log_collector,
            counters: None,
            concurrency_semaphore: common.concurrency_semaphore,
            kiro: common.kiro,
            ip_filter: common.ip_filter,
            site_guard: common.site_guard,
            metrics: common.metrics,
        }
    }

    /// Create a new AppState with shared active_provider and registry (Tauri mode).
    /// This allows the Tauri layer and proxy-core to share the same ArcSwap instances.
    pub fn new_shared(
        config: Config,
        active_provider: Arc<ArcSwap<ProviderConfig>>,
        registry: Arc<ArcSwap<ProviderRegistry>>,
        model_routes: Arc<ArcSwap<Vec<ModelRoute>>>,
        model_routes_enabled: Arc<AtomicBool>,
        log_collector: Arc<LogCollector>,
    ) -> Self {
        let client = build_upstream_client();

        let common = CommonStateParts::build(&config, &client);

        Self {
            config: Arc::new(config),
            active_provider,
            registry,
            model_routes,
            model_routes_enabled,
            client,
            log_collector,
            counters: None,
            concurrency_semaphore: common.concurrency_semaphore,
            kiro: common.kiro,
            ip_filter: common.ip_filter,
            site_guard: common.site_guard,
            metrics: common.metrics,
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

    /// 检查模型路由是否已启用
    pub fn is_model_routes_enabled(&self) -> bool {
        self.model_routes_enabled.load(Ordering::Relaxed)
    }

    /// 设置模型路由启用状态
    pub fn set_model_routes_enabled(&self, enabled: bool) {
        self.model_routes_enabled.store(enabled, Ordering::Relaxed);
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
    pub fn start_background_scheduler(&self) {
        if let Some(ref kiro) = self.kiro {
            kiro.start_background_scheduler();
        }
    }

    // ---- Kiro convenience accessors (backward-compatible) ----

    /// Access the Kiro single-account auth manager (if available).
    pub fn kiro_auth(
        &self,
    ) -> Option<&Arc<tokio::sync::Mutex<crate::convert::kiro::auth::KiroAuthManager>>> {
        self.kiro.as_ref().and_then(|k| k.auth.as_ref())
    }

    /// Access the Kiro multi-account manager (if available).
    pub fn kiro_account_manager(
        &self,
    ) -> Option<&Arc<tokio::sync::Mutex<crate::convert::kiro::account::AccountManager>>> {
        self.kiro.as_ref().and_then(|k| k.account_manager.as_ref())
    }

    /// Access the flow monitor (if Kiro is configured).
    pub fn flow_monitor(
        &self,
    ) -> Option<&Arc<tokio::sync::Mutex<crate::convert::kiro::flow_monitor::FlowMonitor>>> {
        self.kiro.as_ref().map(|k| &k.flow_monitor)
    }

    /// Access the rate limiter (if Kiro is configured).
    pub fn rate_limiter(
        &self,
    ) -> Option<&Arc<tokio::sync::Mutex<crate::convert::kiro::rate_limiter::RateLimiter>>> {
        self.kiro.as_ref().map(|k| &k.rate_limiter)
    }

    /// Access the endpoint health tracker (if Kiro is configured).
    pub fn endpoint_health(
        &self,
    ) -> Option<&crate::convert::kiro::endpoint_health::EndpointHealthTracker> {
        self.kiro.as_ref().map(|k| &k.endpoint_health)
    }

    /// Access the model cache (if Kiro is configured).
    pub fn model_cache(
        &self,
    ) -> Option<&Arc<tokio::sync::Mutex<(Instant, serde_json::Value)>>> {
        self.kiro.as_ref().map(|k| &k.model_cache)
    }
}
