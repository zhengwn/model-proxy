use arc_swap::ArcSwap;
use reqwest::Client;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use tracing::info;

use crate::config::{Config, ConfigError, ModelRoute, ProviderConfig};
use crate::logging::LogCollector;
use crate::provider_registry::ProviderRegistry;
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

        // Initialize Kiro auth manager if any provider uses Kiro format
        let kiro_auth = config.providers.iter().find(|p| p.format == crate::config::ProviderFormat::Kiro)
            .and_then(|p| p.kiro_config.as_ref())
            .map(|kiro_config| {
                let auth = crate::convert::kiro::auth::KiroAuthManager::new(kiro_config, client.clone());
                Arc::new(tokio::sync::Mutex::new(auth))
            });

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

        // Initialize Kiro auth manager if any provider uses Kiro format
        let kiro_auth = config.providers.iter().find(|p| p.format == crate::config::ProviderFormat::Kiro)
            .and_then(|p| p.kiro_config.as_ref())
            .map(|kiro_config| {
                let auth = crate::convert::kiro::auth::KiroAuthManager::new(kiro_config, client.clone());
                Arc::new(tokio::sync::Mutex::new(auth))
            });

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
}
