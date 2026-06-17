pub mod config;
pub mod convert;
pub mod error;
pub mod logging;
pub mod provider_registry;
pub mod server;

pub use provider_registry::ProviderRegistry;
pub use server::AppState;

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
use crate::server::{event_logging_batch, proxy_chat_completions, proxy_count_tokens, proxy_flows, proxy_kiro_login_poll, proxy_kiro_login_start, proxy_kiro_social_exchange, proxy_kiro_social_start, proxy_messages, proxy_models, proxy_responses, proxy_status, proxy_usage};
use crate::server::ip_filter::ip_filter_middleware;
use crate::server::request_id::request_id_middleware;
use crate::server::site_guard::site_guard_middleware;
use axum::middleware;

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

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> axum::response::Response {
    let body = match &state.metrics {
        Some(m) => m.render(),
        None => "# No metrics configured\n".to_string(),
    };
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| {
            axum::response::Response::new(axum::body::Body::empty())
        })
}

/// Start the proxy server with the given config and cancellation token.
///
/// Returns a `JoinHandle` that resolves when the server shuts down.
/// The server listens on `{config.server.host}:{config.server.port}`
/// (host defaults to `127.0.0.1`) and will gracefully shut down when the
/// cancellation token is cancelled.
pub fn start_server(config: Config, token: CancellationToken) -> JoinHandle<()> {
    let port = config.server.port;
    let state = AppState::new(config);
    state.start_background_scheduler();

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
    state.start_background_scheduler();
    start_server_with_state(state, port, token)
}

fn start_server_with_state(state: AppState, port: u16, token: CancellationToken) -> JoinHandle<()> {
    // Resolve bind address from config (secure default: 127.0.0.1).
    let host = state.config.server.host.clone();
    let api_key_set = state.config.server.api_key.is_some();
    let bind_ip: std::net::IpAddr = host.parse().unwrap_or_else(|_| {
        tracing::warn!(
            host = host.as_str(),
            "无法解析监听地址，回退到 127.0.0.1（仅本机访问）"
        );
        std::net::IpAddr::from([127, 0, 0, 1])
    });

    // Security warning: binding to a non-loopback address without an API key
    // exposes the proxy (and any upstream credentials) to the local network.
    if !bind_ip.is_loopback() && !api_key_set {
        tracing::warn!(
            host = host.as_str(),
            "代理监听在非本机地址且未配置 server.api_key——任何能访问该地址的客户端都可使用本代理。强烈建议设置 api_key 或改回 127.0.0.1"
        );
    }

    // Build admin routes (has its own auth middleware)
    let admin_routes = server::admin::admin_router(state.clone());

    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics_handler))
        .route("/v1/messages", post(proxy_messages))
        .route("/v1/messages/count_tokens", post(proxy_count_tokens))
        .route("/cc/v1/messages", post(proxy_messages))
        .route("/cc/v1/messages/count_tokens", post(proxy_count_tokens))
        .route("/v1/chat/completions", post(proxy_chat_completions))
        .route("/v1/models", get(proxy_models))
        .route("/v1/responses", post(proxy_responses))
        .route("/api/status", get(proxy_status))
        .route("/api/usage", get(proxy_usage))
        .route("/api/flows", get(proxy_flows))
        .route("/api/kiro/login/start", post(proxy_kiro_login_start))
        .route("/api/kiro/login/poll", post(proxy_kiro_login_poll))
        .route("/api/kiro/social/start", post(proxy_kiro_social_start))
        .route("/api/kiro/social/exchange", post(proxy_kiro_social_exchange))
        .route("/api/event_logging/batch", post(event_logging_batch))
        .merge(admin_routes)
        // Client auth middleware (skips /health, /metrics, /v1/models)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            server::client_auth_middleware,
        ))
        // Request ID middleware (generates or forwards X-Request-ID)
        .layer(middleware::from_fn(request_id_middleware))
        // IP filter middleware (rejects banned IPs, records request counts)
        .layer(middleware::from_fn_with_state(
            state.ip_filter.clone(),
            ip_filter_middleware,
        ))
        // Site guard middleware (maintenance mode, self-use mode)
        .layer(middleware::from_fn_with_state(
            state.site_guard.clone(),
            site_guard_middleware,
        ))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::new(bind_ip, port);

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
