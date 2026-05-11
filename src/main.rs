mod config;
mod error;
mod proxy;

use axum::{routing::get, routing::post, Json, Router};
use serde_json::json;
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;
use tracing::{error, info};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::{
    config::Config,
    proxy::{event_logging_batch, proxy_messages, AppState},
};

fn stdout_logging_enabled() -> bool {
    std::env::var("MODEL_PROXY_STDOUT_LOG")
        .map(|value| matches!(value.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn init_logging() {
    let log_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("logs");

    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("创建日志目录失败: {}", e);
    }

    let file_appender = tracing_appender::rolling::daily(&log_dir, "model-proxy");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // 泄漏 guard，确保日志在进程生命周期内不会丢失
    let _ = Box::leak(Box::new(guard));

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "model_proxy=info,tower_http=debug".into());

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_ansi(false);

    if stdout_logging_enabled() {
        tracing_subscriber::registry()
            .with(file_layer)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
            .with(filter)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(file_layer)
            .with(filter)
            .init();
    }
}

#[tokio::main]
async fn main() {
    init_logging();

    info!("Model Proxy 启动中...");

    // 加载配置
    let config = match Config::load() {
        Ok(c) => {
            info!(
                "配置加载成功: port={}, provider={}",
                c.server.port, c.provider.base_url
            );
            c
        }
        Err(e) => {
            eprintln!("加载配置失败: {}", e);
            std::process::exit(1);
        }
    };

    let port = config.server.port;
    let state = AppState::new(config);

    // 构建路由
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/messages", post(proxy_messages))
        .route("/api/event_logging/batch", post(event_logging_batch))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("监听地址: http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("绑定端口 {} 失败: {}", port, e);
            std::process::exit(1);
        });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| {
            eprintln!("服务运行错误: {}", e);
            std::process::exit(1);
        });
}

#[cfg(unix)]
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("监听 SIGINT 失败: {}", e);
        }
        "SIGINT"
    };

    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
                "SIGTERM"
            }
            Err(e) => {
                error!("监听 SIGTERM 失败: {}", e);
                std::future::pending::<&'static str>().await
            }
        }
    };

    let signal = tokio::select! {
        signal = ctrl_c => signal,
        signal = terminate => signal,
    };
    info!(signal, "收到退出信号，正在优雅关闭...");
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("监听 SIGINT 失败: {}", e);
    }
    info!(signal = "SIGINT", "收到退出信号，正在优雅关闭...");
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}
