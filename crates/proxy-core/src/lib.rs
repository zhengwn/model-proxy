pub mod config;
pub mod error;
pub mod logging;
pub mod provider_registry;
pub mod proxy;

pub use provider_registry::ProviderRegistry;
pub use proxy::AppState;

use axum::{routing::get, routing::post, Json, Router};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::config::Config;
use crate::proxy::{event_logging_batch, proxy_chat_completions, proxy_messages};

pub use tokio_util::sync::CancellationToken as ServerCancellationToken;

/// Shared request counters for tracking total and failed requests.
#[derive(Debug, Clone)]
pub struct RequestCounters {
    pub total: Arc<AtomicU64>,
    pub failed: Arc<AtomicU64>,
}

impl RequestCounters {
    pub fn new() -> Self {
        Self {
            total: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Default for RequestCounters {
    fn default() -> Self {
        Self::new()
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

/// Start the proxy server with the given config and cancellation token.
///
/// Returns a `JoinHandle` that resolves when the server shuts down.
/// The server will listen on `0.0.0.0:{config.server.port}` and will
/// gracefully shut down when the cancellation token is cancelled.
pub fn start_server(config: Config, token: CancellationToken) -> JoinHandle<()> {
    let port = config.server.port;
    let state = AppState::new(config);

    start_server_with_state(state, port, token)
}

/// Start the proxy server with a pre-built AppState (shared mode for Tauri).
pub fn start_server_shared(
    mut state: AppState,
    port: u16,
    token: CancellationToken,
    counters: Option<RequestCounters>,
) -> JoinHandle<()> {
    if let Some(c) = counters {
        state.set_counters(c);
    }
    start_server_with_state(state, port, token)
}

fn start_server_with_state(state: AppState, port: u16, token: CancellationToken) -> JoinHandle<()> {
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/messages", post(proxy_messages))
        .route("/v1/chat/completions", post(proxy_chat_completions))
        .route("/api/event_logging/batch", post(event_logging_batch))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    tokio::spawn(async move {
        info!("监听地址: http://{}", addr);

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("绑定端口 {} 失败: {}", port, e);
                return;
            }
        };

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                token.cancelled().await;
                info!("收到取消信号，正在优雅关闭代理服务...");
            })
            .await
            .unwrap_or_else(|e| {
                tracing::error!("服务运行错误: {}", e);
            });

        info!("代理服务已关闭");
    })
}

/// Stop the proxy server by cancelling the token and awaiting the handle.
///
/// This function cancels the given token (triggering graceful shutdown)
/// and then waits for the server task to complete.
pub async fn stop_server(token: CancellationToken, handle: JoinHandle<()>) {
    token.cancel();
    let _ = handle.await;
}
