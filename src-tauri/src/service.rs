use proxy_core::config::Config;
use proxy_core::proxy::AppState as ProxyCoreAppState;
use proxy_core::RequestCounters;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Service status returned to the frontend via IPC.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub running: bool,
    pub listen_addr: Option<String>,
    pub started_at: Option<String>,
    pub total_requests: u64,
    pub failed_requests: u64,
    pub error_message: Option<String>,
}

/// Internal state holding the running service handle and metadata.
struct ServiceInner {
    running: bool,
    listen_addr: Option<String>,
    started_at: Option<String>,
    error_message: Option<String>,
    handle: Option<JoinHandle<()>>,
    token: Option<CancellationToken>,
    counters: Option<RequestCounters>,
}

impl ServiceInner {
    fn new() -> Self {
        Self {
            running: false,
            listen_addr: None,
            started_at: None,
            error_message: None,
            handle: None,
            token: None,
            counters: None,
        }
    }

    fn status(&self) -> ServiceStatus {
        let (total_requests, failed_requests) = match &self.counters {
            Some(c) => (
                c.total.load(Ordering::Relaxed),
                c.failed.load(Ordering::Relaxed),
            ),
            None => (0, 0),
        };
        ServiceStatus {
            running: self.running,
            listen_addr: self.listen_addr.clone(),
            started_at: self.started_at.clone(),
            total_requests,
            failed_requests,
            error_message: self.error_message.clone(),
        }
    }
}

/// Thread-safe service manager for the proxy service.
///
/// Manages the lifecycle of the proxy server running as a tokio task.
/// Uses `Arc<Mutex<>>` internally so it can be safely shared as Tauri managed state.
#[derive(Clone)]
pub struct ServiceManager {
    inner: Arc<Mutex<ServiceInner>>,
}

impl ServiceManager {
    /// Create a new `ServiceManager` with no running service.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ServiceInner::new())),
        }
    }

    /// Start the proxy service with the given configuration.
    ///
    /// Creates a `CancellationToken`, spawns the proxy server via `proxy_core::start_server`,
    /// and updates the internal state to reflect the running service.
    pub async fn start(&self, config: Config) -> Result<(), String> {
        let mut inner = self.inner.lock().await;

        if inner.running {
            return Err("服务已在运行中".to_string());
        }

        let port = config.server.port;
        let token = CancellationToken::new();
        let handle = proxy_core::start_server(config, token.clone());

        let listen_addr = format!("0.0.0.0:{}", port);
        let started_at = iso_now();

        inner.running = true;
        inner.listen_addr = Some(listen_addr.clone());
        inner.started_at = Some(started_at);
        inner.error_message = None;
        inner.handle = Some(handle);
        inner.token = Some(token);
        inner.counters = Some(RequestCounters::new());

        info!("代理服务已启动，监听 {}", listen_addr);
        Ok(())
    }

    /// Start the proxy service with a pre-built shared AppState.
    ///
    /// This allows the Tauri layer and proxy-core to share the same ArcSwap
    /// instances, so that switch_provider immediately affects the running proxy.
    /// Accepts an external `CancellationToken` so callers can share it with
    /// background tasks (e.g., FileLogger, EventEmitter).
    pub async fn start_shared(
        &self,
        state: ProxyCoreAppState,
        port: u16,
        cancel_token: Option<CancellationToken>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;

        if inner.running {
            return Err("服务已在运行中".to_string());
        }

        let counters = RequestCounters::new();
        let token = cancel_token.unwrap_or_default();
        let handle =
            proxy_core::start_server_shared(state, port, token.clone(), Some(counters.clone()));

        let listen_addr = format!("0.0.0.0:{}", port);
        let started_at = iso_now();

        inner.running = true;
        inner.listen_addr = Some(listen_addr.clone());
        inner.started_at = Some(started_at);
        inner.error_message = None;
        inner.handle = Some(handle);
        inner.token = Some(token);
        inner.counters = Some(counters);

        info!("代理服务已启动（共享模式），监听 {}", listen_addr);
        Ok(())
    }

    /// Stop the proxy service.
    ///
    /// Cancels the token to trigger graceful shutdown, awaits the server task,
    /// and updates the internal state.
    pub async fn stop(&self) -> Result<(), String> {
        let (token, handle) = {
            let mut inner = self.inner.lock().await;

            if !inner.running {
                return Err("服务未在运行".to_string());
            }

            let token = inner
                .token
                .take()
                .ok_or_else(|| "无法获取取消令牌".to_string())?;
            let handle = inner
                .handle
                .take()
                .ok_or_else(|| "无法获取服务句柄".to_string())?;
            (token, handle)
        };

        // Stop the server outside the lock to avoid holding it during await
        proxy_core::stop_server(token, handle).await;

        let mut inner = self.inner.lock().await;
        inner.running = false;
        inner.listen_addr = None;
        inner.error_message = None;

        info!("代理服务已停止");
        Ok(())
    }

    /// Get the current service status.
    pub async fn get_status(&self) -> ServiceStatus {
        let inner = self.inner.lock().await;
        inner.status()
    }
}

impl Default for ServiceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate an ISO 8601 timestamp string for the current UTC time.
fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proxy_core::config::{
        Config, FallbackConfig, ProviderConfig, ProviderFormat, ProviderQuirks, ServerConfig,
    };
    use proxy_core::logging::LogConfig;

    fn test_config(port: u16) -> Config {
        Config {
            server: ServerConfig {
                port,
                ..Default::default()
            },
            provider: ProviderConfig::placeholder(),
            active_provider: Some("test".to_string()),
            providers: vec![ProviderConfig {
                name: "test".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                api_key: "key".to_string(),
                model: "model".to_string(),
                format: ProviderFormat::Openai,
                quirks: ProviderQuirks::default(),
                model_routes: Vec::new(),
            }],
            model_routes: Vec::new(),
            logging: LogConfig::default(),
            fallback: FallbackConfig::default(),
        }
    }

    #[tokio::test]
    async fn new_service_manager_is_not_running() {
        let manager = ServiceManager::new();
        let status = manager.get_status().await;
        assert!(!status.running);
        assert_eq!(status.total_requests, 0);
        assert_eq!(status.failed_requests, 0);
        assert!(status.listen_addr.is_none());
        assert!(status.started_at.is_none());
    }

    #[tokio::test]
    async fn start_sets_running_state() {
        let manager = ServiceManager::new();
        let port = find_available_port();
        let config = test_config(port);

        manager.start(config).await.unwrap();

        let status = manager.get_status().await;
        assert!(status.running);
        assert_eq!(status.listen_addr, Some(format!("0.0.0.0:{}", port)));
        assert!(status.started_at.is_some());

        // Cleanup
        manager.stop().await.unwrap();
    }

    #[tokio::test]
    async fn double_start_returns_error() {
        let manager = ServiceManager::new();
        let port = find_available_port();
        let config = test_config(port);

        manager.start(config.clone()).await.unwrap();
        let result = manager.start(config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("已在运行"));

        manager.stop().await.unwrap();
    }

    #[tokio::test]
    async fn stop_when_not_running_returns_error() {
        let manager = ServiceManager::new();
        let result = manager.stop().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("未在运行"));
    }

    #[tokio::test]
    async fn stop_clears_running_state() {
        let manager = ServiceManager::new();
        let port = find_available_port();
        let config = test_config(port);

        manager.start(config).await.unwrap();
        manager.stop().await.unwrap();

        let status = manager.get_status().await;
        assert!(!status.running);
        assert!(status.listen_addr.is_none());
    }

    #[tokio::test]
    async fn start_stop_start_works() {
        let manager = ServiceManager::new();
        let port1 = find_available_port();
        let port2 = find_available_port();

        manager.start(test_config(port1)).await.unwrap();
        manager.stop().await.unwrap();
        manager.start(test_config(port2)).await.unwrap();

        let status = manager.get_status().await;
        assert!(status.running);
        assert_eq!(status.listen_addr, Some(format!("0.0.0.0:{}", port2)));

        manager.stop().await.unwrap();
    }

    #[test]
    fn iso_now_produces_valid_format() {
        let ts = iso_now();
        // Should match YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    fn find_available_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }
}
