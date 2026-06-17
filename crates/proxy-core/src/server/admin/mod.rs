//! Admin API for runtime credential management and monitoring.
//!
//! Provides CRUD endpoints for Kiro credentials, force refresh, balance queries,
//! and runtime configuration updates. Protected by `admin_api_key`.

mod credentials;
mod ip;
mod settings;
mod site;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Serialize;
use subtle::ConstantTimeEq;
use tracing::warn;

use super::state::AppState;



// ---- Response types ----

#[derive(Serialize)]
pub(crate) struct AdminSuccess {
    success: bool,
    message: String,
}

#[derive(Serialize)]
struct AdminErrorBody {
    error: AdminErrorDetail,
}

#[derive(Serialize)]
struct AdminErrorDetail {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

// ---- Helper ----

fn admin_error(status: StatusCode, error_type: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(AdminErrorBody {
            error: AdminErrorDetail {
                error_type: error_type.to_string(),
                message: message.into(),
            },
        }),
    )
        .into_response()
}

fn admin_success(message: impl Into<String>) -> Json<AdminSuccess> {
    Json(AdminSuccess {
        success: true,
        message: message.into(),
    })
}

// ---- Middleware ----

/// Check admin auth. Returns None if OK, Some(error_response) if unauthorized.
fn check_admin_auth(headers: &HeaderMap, state: &AppState) -> Option<Response> {
    let expected_key = match state.config.server.admin_api_key {
        Some(ref key) => key,
        None => {
            return Some(admin_error(
                StatusCode::FORBIDDEN,
                "authentication_error",
                "Admin API key not configured",
            ))
        }
    };

    let provided = headers
        .get("x-api-key")
        .or_else(|| headers.get("authorization"))
        .and_then(|v| v.to_str().ok());

    let provided_clean = provided.map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim());

    let is_valid = match provided_clean {
        Some(key) => {
            let key_bytes = key.as_bytes();
            let expected_bytes = expected_key.as_bytes();
            key_bytes.len() == expected_bytes.len() && key_bytes.ct_eq(expected_bytes).into()
        }
        None => false,
    };

    if !is_valid {
        warn!("Admin API key 验证失败");
        return Some(admin_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Unauthorized",
        ));
    }
    None
}

/// Axum middleware that enforces admin API key authentication on all admin routes.
async fn admin_auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(err_response) = check_admin_auth(&headers, &state) {
        return err_response;
    }
    next.run(req).await
}

// ---- Router ----

/// Build the admin API router. All routes are protected by admin API key authentication.
pub fn admin_router(state: AppState) -> Router<AppState> {
    use credentials::*;
    use ip::*;
    use settings::*;
    use site::*;

    Router::new()
        .route(
            "/api/admin/credentials",
            get(admin_list_credentials).post(admin_add_credential),
        )
        .route(
            "/api/admin/credentials/{id}",
            delete(admin_delete_credential),
        )
        .route(
            "/api/admin/credentials/{id}/disabled",
            post(admin_set_disabled),
        )
        .route(
            "/api/admin/credentials/{id}/priority",
            post(admin_set_priority),
        )
        .route(
            "/api/admin/credentials/{id}/reset",
            post(admin_reset_failures),
        )
        .route(
            "/api/admin/credentials/{id}/refresh",
            post(admin_force_refresh),
        )
        .route(
            "/api/admin/credentials/{id}/balance",
            get(admin_get_balance),
        )
        .route(
            "/api/admin/config",
            get(admin_get_config).put(admin_set_config),
        )
        .route(
            "/api/admin/ip/ban",
            post(admin_ip_ban),
        )
        .route(
            "/api/admin/ip/unban",
            post(admin_ip_unban),
        )
        .route(
            "/api/admin/ip/list",
            get(admin_ip_list),
        )
        .route(
            "/api/admin/site/maintenance",
            post(admin_toggle_maintenance),
        )
        .route(
            "/api/admin/site/self-use",
            post(admin_toggle_self_use),
        )
        .route(
            "/api/admin/site/status",
            get(admin_get_site_status),
        )
        // ---- New endpoints for kiro-go alignment ----
        .route(
            "/api/admin/credentials/batch",
            post(admin_batch_credentials),
        )
        .route(
            "/api/admin/credentials/{id}/test",
            post(admin_test_credential),
        )
        .route(
            "/api/admin/credentials/{id}/full",
            get(admin_get_credential_full),
        )
        .route(
            "/api/admin/thinking",
            get(admin_get_thinking).post(admin_set_thinking),
        )
        .route(
            "/api/admin/settings",
            get(admin_get_settings).post(admin_set_settings),
        )
        .route(
            "/api/admin/endpoints/health",
            get(admin_get_endpoint_health),
        )
        .layer(middleware::from_fn_with_state(state, admin_auth_middleware))
}
