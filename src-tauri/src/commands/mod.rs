// Tauri IPC commands for configuration management and service control.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;

use proxy_core::config::{Config, ModelRoute, ProviderConfig, ServerConfig};
use proxy_core::logging::{LogCollector, LogConfig};
use proxy_core::AppState as ProxyCoreAppState;
use proxy_core::ProviderRegistry;

use crate::logging::event_emitter_task;
use crate::service::{ServiceManager, ServiceStatus};

mod kiro;

// Kiro management commands live in the `kiro` submodule but are re-exported here
// so external paths (`commands::kiro_*`) and the Tauri handler registration stay
// unchanged. A glob re-export is required so the `#[tauri::command]`-generated
// `__cmd__*` items are also brought into scope for `generate_handler!`.
pub use kiro::*;

/// Validate that a URL does not point to localhost or private IP ranges (SSRF protection).
fn validate_url_not_local(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid URL".to_string())?;
    let host = parsed.host_str().ok_or("URL has no host")?;
    let host_lower = host.to_lowercase();

    // Block localhost variants
    if host_lower == "localhost"
        || host_lower == "127.0.0.1"
        || host_lower == "::1"
        || host_lower == "[::1]"
        || host_lower == "0.0.0.0"
    {
        return Err("Cannot test providers pointing to localhost".to_string());
    }

    // Block private IP ranges (10.x, 172.16-31.x, 192.168.x)
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                let octets = v4.octets();
                if octets[0] == 10
                    || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                    || (octets[0] == 192 && octets[1] == 168)
                    || octets[0] == 127
                    || octets[0] == 0
                {
                    return Err("Cannot test providers pointing to private/local IPs".to_string());
                }
            }
            std::net::IpAddr::V6(v6) => {
                if v6.is_loopback() || v6.is_unspecified() {
                    return Err("Cannot test providers pointing to local IPs".to_string());
                }
            }
        }
    }

    Ok(())
}



/// Response payload for the `get_providers` command.
#[derive(Debug, Clone, Serialize)]
pub struct ProvidersInfo {
    pub providers: Vec<ProviderConfig>,
    pub active_provider: String,
}

/// Application state holding multi-provider configuration.
///
/// The `active_provider` and `registry` are shared with the proxy-core AppState
/// via Arc, so changes here immediately affect the running proxy service.
pub struct TauriState {
    /// Path to the TOML config file for persistence.
    pub config_path: PathBuf,
    /// Registry of all configured providers (swappable at runtime).
    pub registry: Arc<ArcSwap<ProviderRegistry>>,
    /// Currently active provider, shared with proxy-core for lock-free switching.
    pub active_provider: Arc<ArcSwap<ProviderConfig>>,
    /// Global model routing rules, shared with proxy-core for hot-reload.
    pub model_routes: Arc<ArcSwap<Vec<ModelRoute>>>,
    /// Whether model routing is enabled (shared with proxy-core).
    pub model_routes_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Mutex to serialize config file read-modify-write operations.
    pub config_lock: Arc<Mutex<()>>,
    /// Logging configuration, shared with LogCollector for hot-reload.
    pub log_config: Arc<ArcSwap<LogConfig>>,
}

/// Read and return the current configuration from the config file.
#[tauri::command]
pub async fn get_config(state: State<'_, TauriState>) -> Result<Config, String> {
    let _lock = state.config_lock.lock().await;
    let mut config = get_config_internal(&state.config_path)?;
    config.normalize();
    Ok(config)
}

/// Validate and save the configuration to the config file.
/// Also updates the in-memory registry and active provider to stay in sync.
#[tauri::command]
pub async fn save_config(state: State<'_, TauriState>, mut config: Config) -> Result<(), String> {
    let _lock = state.config_lock.lock().await;
    let path = &state.config_path;

    // Normalize and validate
    config.normalize();
    config
        .validate()
        .map_err(|e| format!("配置验证失败: {}", e))?;

    // Persist to file
    persist_config(path, &config)?;

    // Update in-memory state to match
    let new_registry = ProviderRegistry::new(config.providers.clone())
        .map_err(|e| format!("构建 Registry 失败: {}", e))?;
    state.registry.store(Arc::new(new_registry));

    // Update active provider
    if let Ok(active) = config.active_provider_config() {
        state.active_provider.store(Arc::new(active.clone()));
    }

    state
        .model_routes
        .store(Arc::new(config.model_routes.clone()));

    state
        .model_routes_enabled
        .store(config.model_routes_enabled, std::sync::atomic::Ordering::Relaxed);

    // Update log config so LogCollector and FileLogger see changes immediately
    state.log_config.store(Arc::new(config.logging.clone()));

    Ok(())
}

/// Save only the server section without requiring providers to be configured.
#[tauri::command]
pub async fn save_server_config(
    state: State<'_, TauriState>,
    server: ServerConfig,
) -> Result<(), String> {
    let _lock = state.config_lock.lock().await;
    persist_server_config(&state.config_path, server)
}

/// Return the path to the configuration file.
#[tauri::command]
pub async fn get_config_path(state: State<'_, TauriState>) -> Result<String, String> {
    Ok(state.config_path.to_string_lossy().to_string())
}

/// Start the proxy service.
///
/// Loads the current configuration and starts the proxy server with shared state,
/// so that switch_provider immediately affects the running proxy.
#[tauri::command]
pub async fn start_service(
    app_handle: AppHandle,
    app_state: State<'_, TauriState>,
    service: State<'_, ServiceManager>,
) -> Result<(), String> {
    let _lock = app_state.config_lock.lock().await;

    // Load config from file
    let mut config = get_config_internal(&app_state.config_path)?;
    config.normalize();
    config
        .validate()
        .map_err(|e| format!("配置验证失败: {}", e))?;

    // Rebuild registry from current config and update shared state
    let new_registry = ProviderRegistry::new(config.providers.clone())
        .map_err(|e| format!("构建 Registry 失败: {}", e))?;
    app_state.registry.store(Arc::new(new_registry));

    if let Ok(active) = config.active_provider_config() {
        app_state.active_provider.store(Arc::new(active.clone()));
    }

    app_state
        .model_routes
        .store(Arc::new(config.model_routes.clone()));

    app_state
        .model_routes_enabled
        .store(config.model_routes_enabled, std::sync::atomic::Ordering::Relaxed);

    // Update the shared log_config from the current config's logging section
    app_state.log_config.store(Arc::new(config.logging.clone()));

    // Create LogCollector with the shared log_config
    let log_collector = Arc::new(LogCollector::new(app_state.log_config.clone(), 256));

    // Subscribe two receivers from the broadcast channel
    let receiver1 = log_collector.sender.subscribe();
    let receiver2 = log_collector.sender.subscribe();

    // Determine app_data_dir for FileLogger
    let app_data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("获取 app_data_dir 失败: {}", e))?;

    // Create a shared cancellation token for graceful shutdown
    let cancel_token = tokio_util::sync::CancellationToken::new();

    // Spawn FileLogger background task
    let file_logger_config = app_state.log_config.clone();
    let file_logger_cancel = cancel_token.clone();
    tokio::spawn(async move {
        proxy_core::logging::FileLogger::run(
            receiver1,
            file_logger_config,
            app_data_dir,
            file_logger_cancel,
        )
        .await;
    });

    // Spawn EventEmitter background task
    let event_emitter_cancel = cancel_token.clone();
    tokio::spawn(async move {
        event_emitter_task(receiver2, app_handle, event_emitter_cancel).await;
    });

    // Start the service with shared ArcSwap instances
    let proxy_state = ProxyCoreAppState::new_shared(
        config,
        app_state.active_provider.clone(),
        app_state.registry.clone(),
        app_state.model_routes.clone(),
        app_state.model_routes_enabled.clone(),
        log_collector,
    );
    let port = proxy_state.config.server.port;

    service
        .start_shared(proxy_state, port, Some(cancel_token))
        .await
}

/// Stop the running proxy service.
#[tauri::command]
pub async fn stop_service(service: State<'_, ServiceManager>) -> Result<(), String> {
    service.stop().await
}

/// Get the current service status.
#[tauri::command]
pub async fn get_service_status(
    service: State<'_, ServiceManager>,
) -> Result<ServiceStatus, String> {
    Ok(service.get_status().await)
}

/// Switch the active provider at runtime.
///
/// Acquires the config lock first to ensure atomicity between in-memory
/// and on-disk state, then updates the shared ArcSwap and persists.
#[tauri::command]
pub async fn switch_provider(app_state: State<'_, TauriState>, name: String) -> Result<(), String> {
    let _lock = app_state.config_lock.lock().await;

    // Validate against current registry
    let registry = app_state.registry.load();
    let provider = registry
        .get(&name)
        .ok_or_else(|| format!("Provider 未找到: {}", name))?;

    // Persist to config file first — if this fails, don't update in-memory state
    persist_active_provider(&app_state.config_path, &name)
        .map_err(|e| format!("持久化失败: {}", e))?;

    // Update shared ArcSwap — this immediately affects the running proxy
    app_state.active_provider.store(Arc::new(provider.clone()));

    Ok(())
}

/// Get all configured providers and the active provider name.
#[tauri::command]
pub async fn get_providers(app_state: State<'_, TauriState>) -> Result<ProvidersInfo, String> {
    let _lock = app_state.config_lock.lock().await;
    let mut config = match get_config_internal(&app_state.config_path) {
        Ok(c) => c,
        Err(e) if e.contains("不存在") => {
            return Ok(ProvidersInfo {
                providers: Vec::new(),
                active_provider: String::new(),
            });
        }
        Err(e) => return Err(e),
    };
    config.normalize();

    let active_provider = config.active_provider.clone().unwrap_or_else(|| {
        config
            .providers
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_default()
    });

    Ok(ProvidersInfo {
        providers: config.providers,
        active_provider,
    })
}

/// Add a new provider to the configuration.
/// Updates both the config file and the in-memory registry.
#[tauri::command]
pub async fn add_provider(
    app_state: State<'_, TauriState>,
    provider: ProviderConfig,
) -> Result<(), String> {
    // Validate required fields
    validate_provider_fields(&provider)?;

    let _lock = app_state.config_lock.lock().await;

    // Read current config, or create a default one if file doesn't exist yet
    let mut config = match get_config_internal(&app_state.config_path) {
        Ok(c) => c,
        Err(e) if e.contains("不存在") => {
            // First launch: create a minimal config
            Config {
                server: proxy_core::config::ServerConfig::default(),
                provider: ProviderConfig::placeholder(),
                active_provider: None,
                providers: Vec::new(),
                model_routes: Vec::new(),
                model_routes_enabled: true,
                logging: proxy_core::logging::LogConfig::default(),
                fallback: proxy_core::config::FallbackConfig::default(),
            }
        }
        Err(e) => return Err(e),
    };
    config.normalize();

    // Validate name uniqueness
    if config.providers.iter().any(|p| p.name == provider.name) {
        return Err(format!("Provider name 重复: {}", provider.name));
    }

    // If this is the first provider, set it as active
    let is_first = config.providers.is_empty();
    let provider_name = provider.name.clone();

    // Append new provider
    config.providers.push(provider);

    if is_first {
        config.active_provider = Some(provider_name.clone());
    }

    // Persist
    persist_config(&app_state.config_path, &config)?;

    // Update in-memory registry
    let new_registry = ProviderRegistry::new(config.providers.clone())
        .map_err(|e| format!("构建 Registry 失败: {}", e))?;
    app_state.registry.store(Arc::new(new_registry));

    // If first provider, also update active_provider in memory
    if is_first {
        if let Some(p) = config.providers.iter().find(|p| p.name == provider_name) {
            app_state.active_provider.store(Arc::new(p.clone()));
        }
    }

    Ok(())
}

/// Update an existing provider's configuration (supports rename).
/// Updates both the config file and the in-memory registry.
#[tauri::command]
pub async fn update_provider(
    app_state: State<'_, TauriState>,
    provider: ProviderConfig,
    original_name: Option<String>,
) -> Result<(), String> {
    // Validate required fields
    validate_provider_fields(&provider)?;

    let _lock = app_state.config_lock.lock().await;

    // Read current config
    let mut config = get_config_internal(&app_state.config_path)?;
    config.normalize();

    let lookup_name = original_name.as_deref().unwrap_or(&provider.name);

    // Find provider index by original name
    let idx = config
        .providers
        .iter()
        .position(|p| p.name == lookup_name)
        .ok_or_else(|| format!("Provider 未找到: {}", lookup_name))?;

    // If renaming, check new name doesn't conflict
    if provider.name != lookup_name
        && config
            .providers
            .iter()
            .enumerate()
            .any(|(i, p)| p.name == provider.name && i != idx)
    {
        return Err(format!("Provider name 重复: {}", provider.name));
    }

    // Update all fields including name
    let existing = &mut config.providers[idx];
    existing.name = provider.name.clone();
    existing.base_url = provider.base_url;
    existing.api_key = provider.api_key;
    existing.model = provider.model;
    existing.format = provider.format;
    existing.quirks = provider.quirks;
    existing.kiro_config = provider.kiro_config;

    // If the renamed provider was the active one, update active_provider reference
    if config.active_provider.as_deref() == Some(lookup_name) {
        config.active_provider = Some(provider.name.clone());
    }

    // Persist
    persist_config(&app_state.config_path, &config)?;

    // Update in-memory registry
    let new_registry = ProviderRegistry::new(config.providers.clone())
        .map_err(|e| format!("构建 Registry 失败: {}", e))?;
    app_state.registry.store(Arc::new(new_registry));

    // If the updated provider is the active one, update the active ArcSwap too
    let active_name = {
        let current = app_state.active_provider.load();
        current.name.clone()
    };
    if active_name == lookup_name || active_name == provider.name {
        if let Some(updated) = config.providers.iter().find(|p| p.name == provider.name) {
            app_state.active_provider.store(Arc::new(updated.clone()));
        }
    }

    Ok(())
}

/// Delete a provider from the configuration.
/// Updates both the config file and the in-memory registry.
/// Allows deleting the last/active provider when the service is not running.
#[tauri::command]
pub async fn delete_provider(
    app_state: State<'_, TauriState>,
    service: State<'_, ServiceManager>,
    name: String,
) -> Result<(), String> {
    let _lock = app_state.config_lock.lock().await;

    // Read current config
    let mut config = get_config_internal(&app_state.config_path)?;
    config.normalize();

    // Check that the provider exists
    if !config.providers.iter().any(|p| p.name == name) {
        return Err(format!("Provider 未找到: {}", name));
    }

    let is_running = service.get_status().await.running;

    // When service is running, don't allow deleting the active or last provider
    if is_running {
        if config.providers.len() <= 1 {
            return Err("服务运行中不能删除最后一个 Provider".to_string());
        }
        let active = config.active_provider.clone().unwrap_or_else(|| {
            config
                .providers
                .first()
                .map(|p| p.name.clone())
                .unwrap_or_default()
        });
        if active == name {
            return Err(
                "服务运行中不能删除当前活跃的 Provider，请先切换到其他 Provider".to_string(),
            );
        }
    }

    // Remove the provider
    config.providers.retain(|p| p.name != name);

    // If we deleted the active provider, clear it
    if config.active_provider.as_deref() == Some(&name) {
        config.active_provider = config.providers.first().map(|p| p.name.clone());
    }

    // Persist
    persist_config(&app_state.config_path, &config)?;

    // Update in-memory registry
    let new_registry = ProviderRegistry::new(config.providers.clone())
        .map_err(|e| format!("构建 Registry 失败: {}", e))?;
    app_state.registry.store(Arc::new(new_registry));

    // Update active provider in memory
    if let Some(ref active_name) = config.active_provider {
        if let Some(p) = config.providers.iter().find(|p| &p.name == active_name) {
            app_state.active_provider.store(Arc::new(p.clone()));
        }
    }

    Ok(())
}

/// Get the global model routes.
#[tauri::command]
pub async fn get_model_routes(app_state: State<'_, TauriState>) -> Result<Vec<ModelRoute>, String> {
    let _lock = app_state.config_lock.lock().await;
    let mut config = get_config_internal(&app_state.config_path)?;
    config.normalize();
    Ok(config.model_routes)
}

/// Save the global model routes.
#[tauri::command]
pub async fn save_model_routes(
    app_state: State<'_, TauriState>,
    routes: Vec<ModelRoute>,
) -> Result<(), String> {
    let _lock = app_state.config_lock.lock().await;
    let mut config = get_config_internal(&app_state.config_path)?;
    config.normalize();
    config.model_routes = routes;
    persist_config(&app_state.config_path, &config)?;
    app_state
        .model_routes
        .store(Arc::new(config.model_routes.clone()));
    Ok(())
}

/// Get whether model routing is enabled.
#[tauri::command]
pub async fn get_model_routes_enabled(app_state: State<'_, TauriState>) -> Result<bool, String> {
    Ok(app_state.model_routes_enabled.load(std::sync::atomic::Ordering::Relaxed))
}

/// Set whether model routing is enabled.
#[tauri::command]
pub async fn set_model_routes_enabled(
    app_state: State<'_, TauriState>,
    enabled: bool,
) -> Result<(), String> {
    let _lock = app_state.config_lock.lock().await;
    let mut config = get_config_internal(&app_state.config_path)?;
    config.normalize();
    config.model_routes_enabled = enabled;
    persist_config(&app_state.config_path, &config)?;
    app_state.model_routes_enabled.store(enabled, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

// === Internal helpers ===

/// Result of a provider connectivity test.
#[derive(Debug, Clone, Serialize)]
pub struct TestProviderResult {
    pub success: bool,
    pub latency_ms: u64,
    pub model: Option<String>,
    pub error: Option<String>,
}

/// Test a provider's connectivity by sending a minimal request.
///
/// Sends a single-message request with max_tokens=1 to verify the API key,
/// base URL, and model are all valid and reachable.
#[tauri::command]
pub async fn test_provider(provider: ProviderConfig) -> Result<TestProviderResult, String> {
    validate_provider_fields(&provider)?;
    validate_url_not_local(&provider.base_url)?;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {}", e))?;

    let start = std::time::Instant::now();

    let (url, body, auth_header) = match provider.format {
        proxy_core::config::ProviderFormat::Openai => {
            let url = proxy_core::convert::openai_chat_completions_url(&provider.base_url);
            let body = serde_json::json!({
                "model": provider.model,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1
            });
            (url, body, format!("Bearer {}", provider.api_key))
        }
        proxy_core::config::ProviderFormat::Anthropic => {
            let url = proxy_core::convert::anthropic_messages_url(&provider.base_url);
            let body = serde_json::json!({
                "model": provider.model,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1
            });
            (url, body, provider.api_key.clone())
        }
        proxy_core::config::ProviderFormat::Kiro => {
            let region = provider.kiro_config.as_ref()
                .and_then(|k| k.api_region.as_deref())
                .or(provider.kiro_config.as_ref().map(|k| k.region.as_str()))
                .unwrap_or("us-east-1");
            let url = format!("https://q.{}.amazonaws.com/getUsageLimits", region);
            let body = serde_json::json!({});
            (url, body, String::new())
        }
    };

    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&body);

    // Set auth headers based on format
    match provider.format {
        proxy_core::config::ProviderFormat::Openai => {
            req = req.header("authorization", &auth_header);
        }
        proxy_core::config::ProviderFormat::Anthropic => {
            req = req
                .header("x-api-key", &provider.api_key)
                .header("anthropic-version", "2023-06-01");
        }
        proxy_core::config::ProviderFormat::Kiro => {
            // Kiro test uses the admin API instead
            return Err("Kiro provider 请使用 Kiro 管理面板中的测试功能".to_string());
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            let error_msg = if e.is_connect() {
                format!("连接失败: {}", e)
            } else if e.is_timeout() {
                "请求超时（5s）".to_string()
            } else {
                format!("请求失败: {}", e)
            };
            return Ok(TestProviderResult {
                success: false,
                latency_ms,
                model: None,
                error: Some(error_msg),
            });
        }
    };

    let latency_ms = start.elapsed().as_millis() as u64;
    let status = resp.status();

    if status.is_success() {
        // Try to extract the model from the response
        let body_text = resp.text().await.unwrap_or_default();
        let model = serde_json::from_str::<serde_json::Value>(&body_text)
            .ok()
            .and_then(|v| {
                v.get("model")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            });

        Ok(TestProviderResult {
            success: true,
            latency_ms,
            model,
            error: None,
        })
    } else {
        let error_text = resp.text().await.unwrap_or_default();
        let error_msg = if error_text.len() > 200 {
            let mut end = 200;
            while !error_text.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            format!("HTTP {} - {}...", status.as_u16(), &error_text[..end])
        } else {
            format!("HTTP {} - {}", status.as_u16(), error_text)
        };
        Ok(TestProviderResult {
            success: false,
            latency_ms,
            model: None,
            error: Some(error_msg),
        })
    }
}

/// Validate that a ProviderConfig has all required fields non-empty.
fn validate_provider_fields(provider: &ProviderConfig) -> Result<(), String> {
    if provider.name.is_empty() {
        return Err("Provider name 不能为空".to_string());
    }
    if provider.base_url.is_empty() {
        return Err(format!(
            "Provider '{}' 缺少必填字段: base_url",
            provider.name
        ));
    }
    if provider.api_key.is_empty() && provider.format != proxy_core::config::ProviderFormat::Kiro {
        return Err(format!(
            "Provider '{}' 缺少必填字段: api_key",
            provider.name
        ));
    }
    if provider.model.is_empty() {
        return Err(format!("Provider '{}' 缺少必填字段: model", provider.name));
    }
    Ok(())
}

/// Persist the active_provider field to the config file.
fn persist_active_provider(config_path: &PathBuf, name: &str) -> Result<(), String> {
    let content =
        std::fs::read_to_string(config_path).map_err(|e| format!("读取配置文件失败: {}", e))?;

    let mut config: Config =
        toml::from_str(&content).map_err(|e| format!("解析配置文件失败: {}", e))?;
    config.normalize();

    config.active_provider = Some(name.to_string());
    config
        .validate()
        .map_err(|e| format!("配置验证失败: {}", e))?;

    let output = config
        .to_toml_string()
        .map_err(|e| format!("序列化配置失败: {}", e))?;

    std::fs::write(config_path, output).map_err(|e| format!("写入配置文件失败: {}", e))?;
    restrict_file_permissions(config_path);

    Ok(())
}

/// Persist only the server settings. This supports first launch, where provider
/// configuration is intentionally absent until the user adds one.
fn persist_server_config(config_path: &PathBuf, server: ServerConfig) -> Result<(), String> {
    let mut config = match get_config_internal(config_path) {
        Ok(c) => c,
        Err(e) if e.contains("不存在") => Config {
            server: ServerConfig::default(),
            provider: ProviderConfig::placeholder(),
            active_provider: None,
            providers: Vec::new(),
            model_routes: Vec::new(),
            model_routes_enabled: true,
            logging: proxy_core::logging::LogConfig::default(),
            fallback: proxy_core::config::FallbackConfig::default(),
        },
        Err(e) => return Err(e),
    };

    config.normalize();
    config.server = server;
    if config.providers.len() == 1
        && config.providers[0].name == "default"
        && config.providers[0].base_url.is_empty()
        && config.providers[0].api_key.is_empty()
        && config.providers[0].model.is_empty()
    {
        config.providers.clear();
        config.active_provider = None;
    }

    persist_config(config_path, &config)
}

/// Persist the full config to the config file atomically (write temp, then rename).
fn persist_config(config_path: &PathBuf, config: &Config) -> Result<(), String> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
    }

    let output = config
        .to_toml_string()
        .map_err(|e| format!("序列化配置失败: {}", e))?;

    let temp_path = config_path.with_extension("toml.tmp");
    std::fs::write(&temp_path, &output).map_err(|e| format!("写入临时配置文件失败: {}", e))?;
    restrict_file_permissions(&temp_path);
    std::fs::rename(&temp_path, config_path).map_err(|e| format!("重命名配置文件失败: {}", e))?;
    restrict_file_permissions(config_path);

    Ok(())
}

/// Restrict a config/secret file to owner-only access (0600 on Unix).
///
/// The config file contains provider API keys, so it should
/// not be readable by other local users. On non-Unix platforms this is a no-op
/// (the file lives under the user's profile directory, which NTFS ACLs protect).
fn restrict_file_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Internal helper to load config from a path.
fn get_config_internal(path: &PathBuf) -> Result<Config, String> {
    if !path.exists() {
        return Err(format!(
            "配置文件不存在: {}。请先保存配置。",
            path.display()
        ));
    }

    let content = std::fs::read_to_string(path).map_err(|e| format!("读取配置文件失败: {}", e))?;

    let config: Config =
        toml::from_str(&content).map_err(|e| format!("解析配置文件失败: {}", e))?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proxy_core::config::{
        Config, FallbackConfig, KiroConfig, ProviderFormat, ProviderQuirks, ServerConfig,
    };
    use proxy_core::logging::LogConfig;
    use tempfile::TempDir;

    fn make_provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            base_url: "https://api.example.com".to_string(),
            api_key: "sk-test-key".to_string(),
            model: "test-model".to_string(),
            format: ProviderFormat::Openai,
            quirks: ProviderQuirks::default(),
            model_routes: Vec::new(),
            kiro_config: None,
        }
    }

    fn make_config(providers: Vec<ProviderConfig>) -> Config {
        Config {
            server: ServerConfig::default(),
            provider: ProviderConfig::placeholder(),
            active_provider: providers.first().map(|p| p.name.clone()),
            providers,
            model_routes: Vec::new(),
            model_routes_enabled: true,
            logging: LogConfig::default(),
            fallback: FallbackConfig::default(),
        }
    }

    // --- validate_provider_fields tests ---

    #[test]
    fn validate_provider_fields_accepts_valid_provider() {
        let provider = make_provider("test");
        assert!(validate_provider_fields(&provider).is_ok());
    }

    #[test]
    fn validate_provider_fields_rejects_empty_name() {
        let mut provider = make_provider("test");
        provider.name = String::new();
        let result = validate_provider_fields(&provider);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("name"));
    }

    #[test]
    fn validate_provider_fields_rejects_empty_base_url() {
        let mut provider = make_provider("test");
        provider.base_url = String::new();
        let result = validate_provider_fields(&provider);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("base_url"));
    }

    #[test]
    fn validate_provider_fields_rejects_empty_api_key() {
        let mut provider = make_provider("test");
        provider.api_key = String::new();
        let result = validate_provider_fields(&provider);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("api_key"));
    }

    #[test]
    fn validate_provider_fields_allows_kiro_empty_api_key() {
        let mut provider = make_provider("kiro");
        provider.api_key = String::new();
        provider.format = ProviderFormat::Kiro;
        provider.kiro_config = Some(KiroConfig {
            auth_method: "social".to_string(),
            refresh_token: Some("refresh-token".to_string()),
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
            first_token_timeout: None,
            streaming_read_timeout: None,
            first_token_max_retries: None,
            quota_cooldown_secs: None,
            health_score_decay: None,
            health_score_recovery: None,
            preferred_endpoint: None,
            endpoint_fallback: None,
            ..Default::default()
        });

        assert!(validate_provider_fields(&provider).is_ok());
    }

    #[test]
    fn validate_provider_fields_rejects_empty_model() {
        let mut provider = make_provider("test");
        provider.model = String::new();
        let result = validate_provider_fields(&provider);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("model"));
    }

    // --- persist_config / get_config_internal tests ---

    #[test]
    fn persist_and_load_config_round_trip() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let mut config = make_config(vec![make_provider("alpha"), make_provider("beta")]);
        config.active_provider = Some("beta".to_string());
        config.normalize();

        persist_config(&config_path, &config).unwrap();

        let loaded = get_config_internal(&config_path).unwrap();
        assert_eq!(loaded.providers.len(), 2);
        assert_eq!(loaded.active_provider, Some("beta".to_string()));
        assert_eq!(loaded.providers[0].name, "alpha");
        assert_eq!(loaded.providers[1].name, "beta");
    }

    #[test]
    fn get_config_internal_returns_error_for_missing_file() {
        let path = PathBuf::from("/nonexistent/path/config.toml");
        let result = get_config_internal(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("不存在"));
    }

    #[test]
    fn persist_config_creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("subdir").join("nested").join("config.toml");

        let config = make_config(vec![make_provider("test")]);
        persist_config(&config_path, &config).unwrap();

        assert!(config_path.exists());
    }

    #[test]
    fn persist_active_provider_updates_only_active_field() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let mut config = make_config(vec![make_provider("a"), make_provider("b")]);
        config.active_provider = Some("a".to_string());
        config.normalize();
        persist_config(&config_path, &config).unwrap();

        // Switch active provider
        persist_active_provider(&config_path, "b").unwrap();

        let loaded = get_config_internal(&config_path).unwrap();
        assert_eq!(loaded.active_provider, Some("b".to_string()));
        // Providers should still be intact
        assert_eq!(loaded.providers.len(), 2);
    }

    #[test]
    fn persist_active_provider_preserves_legacy_provider_config() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        std::fs::write(
            &config_path,
            r#"
[server]
port = 4000

[provider]
base_url = "https://api.example.com"
api_key = "sk-test"
model = "legacy-model"
format = "openai"
"#,
        )
        .unwrap();

        persist_active_provider(&config_path, "default").unwrap();

        let mut loaded = get_config_internal(&config_path).unwrap();
        loaded.normalize();
        assert_eq!(loaded.active_provider, Some("default".to_string()));
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].name, "default");
        assert_eq!(loaded.providers[0].base_url, "https://api.example.com");
        assert_eq!(loaded.providers[0].model, "legacy-model");
    }

    #[test]
    fn persist_server_config_creates_first_launch_config_without_provider() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let server = ServerConfig {
            port: 5050,
            api_key: Some("local-secret".to_string()),
            ..ServerConfig::default()
        };

        persist_server_config(&config_path, server).unwrap();

        let loaded = get_config_internal(&config_path).unwrap();
        assert_eq!(loaded.server.port, 5050);
        assert_eq!(loaded.server.api_key.as_deref(), Some("local-secret"));
        assert!(loaded.providers.is_empty());
        assert_eq!(loaded.active_provider, None);
    }

    #[test]
    fn persist_server_config_preserves_existing_providers() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = make_config(vec![make_provider("alpha")]);
        persist_config(&config_path, &config).unwrap();

        let server = ServerConfig {
            port: 5051,
            api_key: None,
            ..ServerConfig::default()
        };

        persist_server_config(&config_path, server).unwrap();

        let loaded = get_config_internal(&config_path).unwrap();
        assert_eq!(loaded.server.port, 5051);
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].name, "alpha");
        assert_eq!(loaded.active_provider, Some("alpha".to_string()));
    }

    #[test]
    fn persist_server_config_removes_empty_default_placeholder_provider() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config {
            server: ServerConfig::default(),
            provider: ProviderConfig::placeholder(),
            active_provider: Some("default".to_string()),
            providers: vec![ProviderConfig {
                name: "default".to_string(),
                ..ProviderConfig::placeholder()
            }],
            model_routes: Vec::new(),
            model_routes_enabled: true,
            logging: LogConfig::default(),
            fallback: FallbackConfig::default(),
        };
        persist_config(&config_path, &config).unwrap();

        persist_server_config(
            &config_path,
            ServerConfig {
                port: 5052,
                ..ServerConfig::default()
            },
        )
        .unwrap();

        let loaded = get_config_internal(&config_path).unwrap();
        assert_eq!(loaded.server.port, 5052);
        assert!(loaded.providers.is_empty());
        assert_eq!(loaded.active_provider, None);
    }

    #[test]
    fn persist_config_overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        // Write initial config
        let config1 = make_config(vec![make_provider("first")]);
        persist_config(&config_path, &config1).unwrap();

        // Overwrite with different config
        let config2 = make_config(vec![make_provider("second"), make_provider("third")]);
        persist_config(&config_path, &config2).unwrap();

        let loaded = get_config_internal(&config_path).unwrap();
        assert_eq!(loaded.providers.len(), 2);
        assert_eq!(loaded.providers[0].name, "second");
    }

    #[test]
    fn get_config_internal_returns_error_for_invalid_toml() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "this is not valid toml [[[").unwrap();

        let result = get_config_internal(&config_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("解析配置文件失败"));
    }
}
