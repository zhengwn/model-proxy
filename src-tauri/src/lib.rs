// Tauri app setup and command registration
//
// ## Auto-start (Optional Feature)
//
// To enable auto-start on login, add the `tauri-plugin-autostart` dependency:
//
// 1. Add to src-tauri/Cargo.toml:
//    tauri-plugin-autostart = "2"
//
// 2. Register the plugin in the builder:
//    .plugin(tauri_plugin_autostart::init(
//        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
//        Some(vec!["--autostart"]),
//    ))
//
// 3. Add permission to capabilities/default.json:
//    "autostart:allow-enable", "autostart:allow-disable", "autostart:allow-is-enabled"
//
// 4. Use from frontend:
//    import { enable, disable, isEnabled } from '@tauri-apps/plugin-autostart';

pub mod commands;
pub mod logging;
pub mod service;
pub mod tray;

use std::sync::Arc;

use arc_swap::ArcSwap;
use tauri::Manager;
use tokio::sync::Mutex;
use tracing::warn;

use proxy_core::config::{Config, ModelRoute, ProviderConfig};
use proxy_core::logging::LogConfig;
use proxy_core::ProviderRegistry;

use commands::AppState;
use service::ServiceManager;

type SharedRegistry = Arc<ArcSwap<ProviderRegistry>>;
type SharedProvider = Arc<ArcSwap<ProviderConfig>>;
type SharedModelRoutes = Arc<ArcSwap<Vec<ModelRoute>>>;
type SharedLogConfig = Arc<ArcSwap<LogConfig>>;
type SharedProviderState = (
    SharedRegistry,
    SharedProvider,
    SharedModelRoutes,
    SharedLogConfig,
);

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! Welcome to Model Proxy.", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            commands::get_config,
            commands::save_config,
            commands::get_config_path,
            commands::start_service,
            commands::stop_service,
            commands::get_service_status,
            commands::switch_provider,
            commands::get_providers,
            commands::add_provider,
            commands::update_provider,
            commands::delete_provider,
            commands::get_model_routes,
            commands::save_model_routes,
            commands::test_provider,
        ])
        .setup(|app| {
            // Determine config path: prefer app_data_dir, fallback to exe directory
            let config_path = if let Ok(app_data) = app.path().app_data_dir() {
                app_data.join("config.toml")
            } else {
                std::env::current_exe()
                    .unwrap_or_default()
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join("config.toml")
            };

            // Try to load config and build registry + active provider.
            // If config doesn't exist yet (first launch), use empty defaults.
            let (registry, active_provider, model_routes, log_config) = if config_path.exists() {
                match load_provider_state(&config_path) {
                    Ok((reg, active, routes, log_cfg)) => (reg, active, routes, log_cfg),
                    Err(e) => {
                        warn!("加载配置失败，使用空 Registry: {}", e);
                        let (reg, active, routes) = empty_provider_state();
                        (
                            reg,
                            active,
                            routes,
                            Arc::new(ArcSwap::from_pointee(LogConfig::default())),
                        )
                    }
                }
            } else {
                let (reg, active, routes) = empty_provider_state();
                (
                    reg,
                    active,
                    routes,
                    Arc::new(ArcSwap::from_pointee(LogConfig::default())),
                )
            };

            // Register managed state
            app.manage(AppState {
                config_path,
                registry,
                active_provider,
                model_routes,
                config_lock: Arc::new(Mutex::new(())),
                log_config,
            });
            app.manage(ServiceManager::new());

            // Create system tray
            tray::create_tray(app.handle())?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Minimize to tray on window close instead of exiting
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Load provider registry and active provider from a config file.
fn load_provider_state(config_path: &std::path::Path) -> Result<SharedProviderState, String> {
    let content =
        std::fs::read_to_string(config_path).map_err(|e| format!("读取配置文件失败: {}", e))?;

    let mut config: Config =
        toml::from_str(&content).map_err(|e| format!("解析配置文件失败: {}", e))?;

    config.normalize();
    config
        .validate()
        .map_err(|e| format!("配置验证失败: {}", e))?;

    let registry = ProviderRegistry::new(config.providers.clone())
        .map_err(|e| format!("构建 ProviderRegistry 失败: {}", e))?;

    let active = config
        .active_provider_config()
        .map_err(|e| format!("获取活跃 Provider 失败: {}", e))?;

    let active_provider = Arc::new(ArcSwap::from_pointee(active.clone()));
    let model_routes = Arc::new(ArcSwap::from_pointee(config.model_routes.clone()));
    let log_config = Arc::new(ArcSwap::from_pointee(config.logging.clone()));

    Ok((
        Arc::new(ArcSwap::from_pointee(registry)),
        active_provider,
        model_routes,
        log_config,
    ))
}

/// Create empty provider state for when no config exists yet.
fn empty_provider_state() -> (SharedRegistry, SharedProvider, SharedModelRoutes) {
    let registry = ProviderRegistry::new(vec![]).expect("空 Registry 不应失败");
    let placeholder = ProviderConfig::placeholder();
    let active_provider = Arc::new(ArcSwap::from_pointee(placeholder));
    let model_routes = Arc::new(ArcSwap::from_pointee(Vec::new()));

    (
        Arc::new(ArcSwap::from_pointee(registry)),
        active_provider,
        model_routes,
    )
}
