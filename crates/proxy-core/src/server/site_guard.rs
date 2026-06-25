//! SiteGuard middleware: maintenance mode and self-use mode.
//!
//! - **Maintenance mode**: returns 503 for all requests except `/health`.
//! - **Self-use mode**: rejects requests not originating from `127.0.0.1`.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ---------- Shared config ----------

/// Thread-safe, runtime-toggleable guard configuration.
#[derive(Clone)]
pub struct SiteGuardConfig {
    pub maintenance_mode: Arc<AtomicBool>,
    pub self_use_mode: Arc<AtomicBool>,
}

impl SiteGuardConfig {
    pub fn new(maintenance_mode: bool, self_use_mode: bool) -> Self {
        Self {
            maintenance_mode: Arc::new(AtomicBool::new(maintenance_mode)),
            self_use_mode: Arc::new(AtomicBool::new(self_use_mode)),
        }
    }

    pub fn is_maintenance(&self) -> bool {
        self.maintenance_mode.load(Ordering::Relaxed)
    }

    pub fn is_self_use(&self) -> bool {
        self.self_use_mode.load(Ordering::Relaxed)
    }

    pub fn set_maintenance(&self, value: bool) {
        self.maintenance_mode.store(value, Ordering::Relaxed);
    }

    pub fn set_self_use(&self, value: bool) {
        self.self_use_mode.store(value, Ordering::Relaxed);
    }
}

impl Default for SiteGuardConfig {
    fn default() -> Self {
        Self::new(false, false)
    }
}

// ---------- axum middleware ----------

/// axum `from_fn_with_state` middleware that applies site guard rules.
///
/// - Maintenance mode: returns 503 for all requests except `/health`.
/// - Self-use mode: rejects requests whose client IP is not a loopback address.
pub async fn site_guard_middleware(
    State(config): State<SiteGuardConfig>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_owned();

    // Maintenance mode: block everything except /health
    if config.is_maintenance() {
        let allowed = path == "/health";
        if !allowed {
            return (StatusCode::SERVICE_UNAVAILABLE, "Service is under maintenance").into_response();
        }
    }

    // Self-use mode: only allow localhost
    if config.is_self_use() {
        let client_ip = extract_client_ip(&req);
        let is_local = client_ip.map(|ip| ip.is_loopback()).unwrap_or(false);
        if !is_local {
            return (
                StatusCode::FORBIDDEN,
                "Self-use mode: only localhost connections allowed",
            )
                .into_response();
        }
    }

    next.run(req).await
}

// ---------- IP extraction ----------

// Reuse the secure IP extraction from ip_filter (no X-Forwarded-For trust by default)
use super::ip_filter::extract_client_ip;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_off() {
        let cfg = SiteGuardConfig::default();
        assert!(!cfg.is_maintenance());
        assert!(!cfg.is_self_use());
    }

    #[test]
    fn toggle_at_runtime() {
        let cfg = SiteGuardConfig::new(false, false);
        cfg.set_maintenance(true);
        assert!(cfg.is_maintenance());
        cfg.set_self_use(true);
        assert!(cfg.is_self_use());
        cfg.set_maintenance(false);
        assert!(!cfg.is_maintenance());
    }
}
