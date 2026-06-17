//! Shared HTTP client construction utilities.
//!
//! Centralizes reqwest::Client configuration to avoid duplication across
//! AppState::new(), AppState::new_shared(), and kiro_handlers.

use reqwest::Client;
use std::time::Duration;

/// Default connect timeout for upstream requests.
pub const UPSTREAM_CONNECT_TIMEOUT_SECS: u64 = 30;
/// Default connection pool idle timeout.
pub const UPSTREAM_POOL_IDLE_TIMEOUT_SECS: u64 = 90;
/// Default max idle connections per host.
pub const UPSTREAM_POOL_MAX_IDLE_PER_HOST: usize = 32;
/// Default TCP keepalive interval.
pub const TCP_KEEPALIVE_SECS: u64 = 60;

/// Build the default HTTP client used for upstream provider requests.
///
/// Configures connection pooling, timeouts, TCP nodelay, and keepalive
/// for optimal proxy performance.
pub fn build_upstream_client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(UPSTREAM_CONNECT_TIMEOUT_SECS))
        .pool_idle_timeout(Duration::from_secs(UPSTREAM_POOL_IDLE_TIMEOUT_SECS))
        .pool_max_idle_per_host(UPSTREAM_POOL_MAX_IDLE_PER_HOST)
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
        .build()
        .expect("构建 HTTP 客户端失败")
}

/// Build an HTTP client with optional proxy support (for Kiro endpoints).
///
/// Unlike the previous implementation, this never silently falls back to a
/// direct connection: if a proxy URL is configured but cannot be parsed or the
/// client cannot be built, an error is returned. Silently dropping the proxy
/// would defeat the privacy/routing intent of configuring one.
///
/// The returned client is configured with the same connection pool settings as
/// the default upstream client so that callers can cache and reuse it.
pub fn build_proxied_client(proxy_url: Option<&str>) -> reqwest::Result<Client> {
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(UPSTREAM_CONNECT_TIMEOUT_SECS))
        .pool_idle_timeout(Duration::from_secs(UPSTREAM_POOL_IDLE_TIMEOUT_SECS))
        .pool_max_idle_per_host(UPSTREAM_POOL_MAX_IDLE_PER_HOST)
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS));

    if let Some(proxy_str) = proxy_url {
        // Propagate proxy parse errors instead of silently falling back to a
        // direct connection.
        let proxy = reqwest::Proxy::all(proxy_str)?;
        builder = builder.proxy(proxy);
    }

    builder.build()
}
