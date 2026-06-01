//! Admin API for runtime credential management and monitoring.
//!
//! Provides CRUD endpoints for Kiro credentials, force refresh, balance queries,
//! and runtime configuration updates. Protected by `admin_api_key`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing::warn;

use super::state::AppState;
use crate::convert::kiro::account::LoadBalancingMode;
use crate::convert::kiro::auth::KiroAuthManager;
use crate::config::KiroConfig;
use crate::server::site_guard::SiteGuardConfig;

// ---- Response types ----

#[derive(Serialize)]
struct AdminSuccess {
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

#[derive(Serialize)]
struct CredentialsStatusResponse {
    total: usize,
    available: usize,
    credentials: Vec<CredentialSnapshotResponse>,
}

#[derive(Serialize)]
struct CredentialSnapshotResponse {
    id: String,
    priority: u32,
    disabled: bool,
    failure_count: u32,
    is_current: bool,
    is_available: bool,
    auth_method: String,
    total_requests: u64,
    successful_requests: u64,
    failed_requests: u64,
    proxy_url: Option<String>,
    region: String,
    health_score: u32,
}

#[derive(Serialize)]
struct BalanceResponse {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription_title: Option<String>,
    current_usage: f64,
    usage_limit: f64,
    remaining: f64,
    usage_percentage: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_reset_at: Option<f64>,
}

#[derive(Serialize)]
struct AdminConfigResponse {
    load_balancing_mode: String,
    account_count: usize,
    rate_limiter_enabled: bool,
    flow_monitor_enabled: bool,
}

// ---- Request types ----

#[derive(Deserialize)]
pub struct AddCredentialRequest {
    pub auth_method: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub priority: Option<u32>,
    pub region: Option<String>,
    pub api_region: Option<String>,
    pub proxy_url: Option<String>,
}

#[derive(Deserialize)]
pub struct SetDisabledRequest {
    pub disabled: bool,
}

#[derive(Deserialize)]
pub struct SetPriorityRequest {
    pub priority: u32,
}

#[derive(Deserialize)]
pub struct SetConfigRequest {
    pub load_balancing_mode: Option<String>,
}

#[derive(Deserialize)]
pub struct IpActionRequest {
    pub ip: IpAddr,
}

#[derive(Deserialize)]
pub struct ToggleRequest {
    pub enabled: bool,
}

#[derive(Serialize)]
struct SiteStatusResponse {
    maintenance_mode: bool,
    self_use_mode: bool,
}

#[derive(Serialize)]
struct IpListResponse {
    banned: Vec<String>,
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

// ---- Handlers ----

pub async fn admin_list_credentials(
    State(state): State<AppState>,
) -> Response {
    let Some(ref mgr) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let mgr = mgr.lock().await;
    let snapshots = mgr.snapshot();
    let available = snapshots.iter().filter(|s| s.is_available && !s.disabled).count();
    let total = snapshots.len();

    let credentials: Vec<CredentialSnapshotResponse> = snapshots
        .into_iter()
        .map(|s| CredentialSnapshotResponse {
            id: s.id,
            priority: s.priority,
            disabled: s.disabled,
            failure_count: s.failure_count,
            is_current: s.is_current,
            is_available: s.is_available,
            auth_method: s.auth_method,
            total_requests: s.total_requests,
            successful_requests: s.successful_requests,
            failed_requests: s.failed_requests,
            proxy_url: s.proxy_url,
            region: s.region,
            health_score: s.health_score,
        })
        .collect();

    Json(CredentialsStatusResponse {
        total,
        available,
        credentials,
    })
    .into_response()
}

pub async fn admin_add_credential(
    State(state): State<AppState>,
    Json(req): Json<AddCredentialRequest>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let auth_method = req.auth_method.unwrap_or_else(|| "social".to_string());
    let region = req.region.unwrap_or_else(|| "us-east-1".to_string());
    let priority = req.priority.unwrap_or(0);

    let config = KiroConfig {
        auth_method,
        refresh_token: req.refresh_token,
        client_id: req.client_id,
        client_secret: req.client_secret,
        profile_arn: None,
        region,
        api_region: req.api_region,
        model_aliases: None,
        hidden_models: None,
        kiro_version: None,
        proxy_url: req.proxy_url,
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
    };

    let id = {
        let mut mgr = mgr_arc.lock().await;
        let id = format!("admin:{}", mgr.account_count());
        mgr.add_account(id.clone(), &config, state.client.clone(), priority);
        id
    };

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "success": true,
            "message": "Credential added",
            "credentialId": id,
        })),
    )
        .into_response()
}

pub async fn admin_delete_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let mut mgr = mgr_arc.lock().await;
    if mgr.remove_account(&id) {
        admin_success(format!("Credential '{}' deleted", id)).into_response()
    } else {
        admin_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("Credential '{}' not found", id),
        )
    }
}

pub async fn admin_set_disabled(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetDisabledRequest>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let mut mgr = mgr_arc.lock().await;
    if mgr.set_disabled(&id, req.disabled) {
        admin_success(format!(
            "Credential '{}' {}",
            id,
            if req.disabled { "disabled" } else { "enabled" }
        ))
        .into_response()
    } else {
        admin_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("Credential '{}' not found", id),
        )
    }
}

pub async fn admin_set_priority(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetPriorityRequest>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let mut mgr = mgr_arc.lock().await;
    if mgr.set_priority(&id, req.priority) {
        admin_success(format!(
            "Credential '{}' priority set to {}",
            id, req.priority
        ))
        .into_response()
    } else {
        admin_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("Credential '{}' not found", id),
        )
    }
}

pub async fn admin_reset_failures(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let mut mgr = mgr_arc.lock().await;
    if mgr.reset_failures(&id) {
        admin_success(format!("Credential '{}' failures reset", id)).into_response()
    } else {
        admin_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("Credential '{}' not found", id),
        )
    }
}

pub async fn admin_force_refresh(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let mgr = mgr_arc.lock().await;
    match mgr.force_refresh_account(&id).await {
        Ok(_token) => admin_success(format!("Credential '{}' token refreshed", id))
            .into_response(),
        Err(e) => admin_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            format!("Token refresh failed: {}", e),
        ),
    }
}

pub async fn admin_get_balance(
    State(state): State<AppState>,
    Path(_id): Path<String>,
) -> Response {
    // Balance query requires an auth token; use the account manager's auth
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    // Get token from account manager
    let token = {
        let mgr = mgr_arc.lock().await;
        match mgr.current_account() {
            Some((_id, auth_arc)) => {
                let mut auth = auth_arc.lock().await;
                match auth.get_valid_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        return admin_error(
                            StatusCode::BAD_GATEWAY,
                            "api_error",
                            format!("Failed to get auth token: {}", e),
                        )
                    }
                }
            }
            None => {
                return admin_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    "No available Kiro accounts",
                )
            }
        }
    };

    // Find the Kiro config for region info
    let kiro_config = state
        .config
        .providers
        .iter()
        .find(|p| p.format == crate::config::ProviderFormat::Kiro)
        .and_then(|p| p.kiro_config.as_ref());

    let region = kiro_config
        .and_then(|c| c.api_region.as_deref().or(Some(c.region.as_str())))
        .unwrap_or("us-east-1");

    let url = format!("https://q.{}.amazonaws.com/getUsageLimits", region);

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("Authorization", format!("Bearer {}", token))
        .header(
            "x-amz-target",
            "AmazonCodeWhispererStreamingService.GetUsageLimits",
        )
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body = r.text().await.unwrap_or_default();
            // Parse and return the balance info
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                Json(json).into_response()
            } else {
                admin_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Failed to parse balance response",
                )
            }
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            admin_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("Upstream error ({}): {}", status, text),
            )
        }
        Err(e) => admin_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            format!("Network error: {}", e),
        ),
    }
}

pub async fn admin_get_config(
    State(state): State<AppState>,
) -> Json<AdminConfigResponse> {
    let mode = if let Some(ref mgr) = state.kiro_account_manager {
        let mgr = mgr.lock().await;
        mgr.load_balancing_mode().as_str().to_string()
    } else {
        "single".to_string()
    };

    let account_count = if let Some(ref mgr) = state.kiro_account_manager {
        let mgr = mgr.lock().await;
        mgr.account_count()
    } else {
        1
    };

    Json(AdminConfigResponse {
        load_balancing_mode: mode,
        account_count,
        rate_limiter_enabled: state.rate_limiter.is_some(),
        flow_monitor_enabled: state.flow_monitor.is_some(),
    })
}

pub async fn admin_set_config(
    State(state): State<AppState>,
    Json(req): Json<SetConfigRequest>,
) -> Response {
    if let Some(mode_str) = req.load_balancing_mode {
        if let Some(ref mgr_arc) = state.kiro_account_manager {
            let mode = LoadBalancingMode::from_str(&mode_str);
            let mut mgr = mgr_arc.lock().await;
            mgr.set_load_balancing_mode(mode.clone());
            Json(AdminConfigResponse {
                load_balancing_mode: mode.as_str().to_string(),
                account_count: mgr.account_count(),
                rate_limiter_enabled: state.rate_limiter.is_some(),
                flow_monitor_enabled: state.flow_monitor.is_some(),
            })
            .into_response()
        } else {
            admin_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Multi-account mode not enabled",
            )
        }
    } else {
        admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "No config changes specified",
        )
    }
}

// ---- IP Admin Handlers ----

pub async fn admin_ip_ban(
    State(state): State<AppState>,
    Json(req): Json<IpActionRequest>,
) -> Response {
    let already_banned = state.ip_filter.is_banned(req.ip);
    state.ip_filter.ban_ip(req.ip);

    if already_banned {
        admin_success(format!("IP {} was already banned", req.ip)).into_response()
    } else {
        (
            StatusCode::CREATED,
            Json(AdminSuccess {
                success: true,
                message: format!("IP {} banned", req.ip),
            }),
        )
            .into_response()
    }
}

pub async fn admin_ip_unban(
    State(state): State<AppState>,
    Json(req): Json<IpActionRequest>,
) -> Response {
    if !state.ip_filter.is_banned(req.ip) {
        return admin_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("IP {} is not banned", req.ip),
        );
    }

    state.ip_filter.unban_ip(req.ip);
    admin_success(format!("IP {} unbanned", req.ip)).into_response()
}

pub async fn admin_ip_list(
    State(state): State<AppState>,
) -> Json<IpListResponse> {
    let banned = state
        .ip_filter
        .list_banned()
        .into_iter()
        .map(|ip| ip.to_string())
        .collect();

    Json(IpListResponse { banned })
}

// ---- Site Guard Admin Handlers ----

pub async fn admin_toggle_maintenance(
    State(state): State<AppState>,
    Json(req): Json<ToggleRequest>,
) -> Response {
    state.site_guard.set_maintenance(req.enabled);
    admin_success(format!(
        "Maintenance mode {}",
        if req.enabled { "enabled" } else { "disabled" }
    ))
    .into_response()
}

pub async fn admin_toggle_self_use(
    State(state): State<AppState>,
    Json(req): Json<ToggleRequest>,
) -> Response {
    state.site_guard.set_self_use(req.enabled);
    admin_success(format!(
        "Self-use mode {}",
        if req.enabled { "enabled" } else { "disabled" }
    ))
    .into_response()
}

pub async fn admin_get_site_status(
    State(state): State<AppState>,
) -> Json<SiteStatusResponse> {
    Json(SiteStatusResponse {
        maintenance_mode: state.site_guard.is_maintenance(),
        self_use_mode: state.site_guard.is_self_use(),
    })
}

/// Build the admin API router.
pub fn admin_router(state: AppState) -> Router<AppState> {
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
}

// ---- New admin handlers ----

#[derive(Deserialize)]
struct BatchCredentialRequest {
    ids: Vec<String>,
    action: String, // "enable" | "disable" | "refresh"
}

async fn admin_batch_credentials(
    State(state): State<AppState>,
    Json(req): Json<BatchCredentialRequest>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let mut results = Vec::new();

    for id in &req.ids {
        let result = match req.action.as_str() {
            "enable" => {
                let mut mgr = mgr_arc.lock().await;
                if mgr.set_disabled(id, false) {
                    format!("{}: enabled", id)
                } else {
                    format!("{}: not found", id)
                }
            }
            "disable" => {
                let mut mgr = mgr_arc.lock().await;
                if mgr.set_disabled(id, true) {
                    format!("{}: disabled", id)
                } else {
                    format!("{}: not found", id)
                }
            }
            "refresh" => {
                let mgr = mgr_arc.lock().await;
                match mgr.force_refresh_account(id).await {
                    Ok(_) => format!("{}: refreshed", id),
                    Err(e) => format!("{}: refresh failed - {}", id, e),
                }
            }
            other => {
                format!("{}: unknown action '{}'", id, other)
            }
        };
        results.push(result);
    }

    Json(serde_json::json!({
        "success": true,
        "results": results,
    }))
    .into_response()
}

async fn admin_test_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    // Get token from the specified account
    let token = {
        let mgr = mgr_arc.lock().await;
        match mgr.account_auth_at_id(&id) {
            Some(auth_arc) => {
                let mut auth = auth_arc.lock().await;
                match auth.get_valid_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        return admin_error(
                            StatusCode::BAD_GATEWAY,
                            "auth_error",
                            format!("Failed to get token: {}", e),
                        );
                    }
                }
            }
            None => {
                return admin_error(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("Credential '{}' not found", id),
                );
            }
        }
    };

    // Send a minimal test request
    let region = {
        let mgr = mgr_arc.lock().await;
        mgr.account_region(&id).unwrap_or_else(|| "us-east-1".to_string())
    };
    let url = format!(
        "https://q.{}.amazonaws.com/generateAssistantResponse",
        region
    );

    let test_payload = serde_json::json!({
        "conversationState": {
            "currentMessage": {
                "userInputMessage": {
                    "content": "say ok",
                    "modelId": "claude-sonnet-4.5",
                    " userInputMessageContext": {}
                }
            },
            "chatTriggerType": "MANUAL"
        }
    });

    let headers = crate::convert::kiro::endpoint::build_endpoint_headers(
        &crate::convert::kiro::endpoint::KIRO_ENDPOINTS[0],
        &token,
        "aws-sdk-js/3.980.0 KiroIDE",
        &format!("KiroIDE-{}-test", "0.11.107"),
    );

    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    let mut req = client.post(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    match req.json(&test_payload).send().await {
        Ok(resp) => {
            let latency = start.elapsed().as_millis() as u64;
            let status = resp.status().as_u16();
            if status == 200 || status == 204 {
                Json(serde_json::json!({
                    "success": true,
                    "latency_ms": latency,
                    "status": status,
                }))
                .into_response()
            } else {
                let body = resp.text().await.unwrap_or_default();
                Json(serde_json::json!({
                    "success": false,
                    "latency_ms": latency,
                    "status": status,
                    "error": body,
                }))
                .into_response()
            }
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "error": format!("Request failed: {}", e),
        }))
        .into_response(),
    }
}

async fn admin_get_credential_full(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(ref mgr_arc) = state.kiro_account_manager else {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Multi-account mode not enabled",
        );
    };

    let mgr = mgr_arc.lock().await;
    match mgr.account_full_snapshot(&id) {
        Some(snapshot) => Json(snapshot).into_response(),
        None => admin_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("Credential '{}' not found", id),
        ),
    }
}

#[derive(Serialize, Deserialize)]
struct ThinkingConfigResponse {
    mode: String,
}

async fn admin_get_thinking(State(state): State<AppState>) -> Response {
    let provider = state.current_provider();
    let mode = provider
        .kiro_config
        .as_ref()
        .and_then(|c| c.thinking_mode.clone())
        .unwrap_or_else(|| "as_reasoning_content".to_string());

    Json(ThinkingConfigResponse { mode }).into_response()
}

async fn admin_set_thinking(
    State(state): State<AppState>,
    Json(req): Json<ThinkingConfigResponse>,
) -> Response {
    let valid_modes = ["as_reasoning_content", "remove", "pass", "strip_tags"];
    if !valid_modes.contains(&req.mode.as_str()) {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!(
                "Invalid thinking mode '{}'. Valid: {:?}",
                req.mode, valid_modes
            ),
        );
    }

    // Update the active provider's kiro_config
    let provider = state.current_provider();
    if let Some(ref kiro_config) = provider.kiro_config {
        let mut new_config = kiro_config.clone();
        new_config.thinking_mode = Some(req.mode.clone());

        let mut new_provider = provider.as_ref().clone();
        new_provider.kiro_config = Some(new_config);

        state.active_provider.store(Arc::new(new_provider));
    }

    admin_success(format!("Thinking mode set to '{}'", req.mode)).into_response()
}

#[derive(Serialize, Deserialize)]
struct ProxySettingsResponse {
    preferred_endpoint: Option<String>,
    endpoint_fallback: Option<bool>,
}

async fn admin_get_settings(State(state): State<AppState>) -> Response {
    let provider = state.current_provider();
    let settings = if let Some(ref kiro_config) = provider.kiro_config {
        ProxySettingsResponse {
            preferred_endpoint: kiro_config.preferred_endpoint.clone(),
            endpoint_fallback: kiro_config.endpoint_fallback,
        }
    } else {
        ProxySettingsResponse {
            preferred_endpoint: None,
            endpoint_fallback: None,
        }
    };

    Json(settings).into_response()
}

async fn admin_set_settings(
    State(state): State<AppState>,
    Json(req): Json<ProxySettingsResponse>,
) -> Response {
    let provider = state.current_provider();
    if let Some(ref kiro_config) = provider.kiro_config {
        let mut new_config = kiro_config.clone();
        if let Some(ep) = req.preferred_endpoint {
            new_config.preferred_endpoint = Some(ep);
        }
        if let Some(fb) = req.endpoint_fallback {
            new_config.endpoint_fallback = Some(fb);
        }

        let mut new_provider = provider.as_ref().clone();
        new_provider.kiro_config = Some(new_config);

        state.active_provider.store(Arc::new(new_provider));
    }

    admin_success("Settings updated".to_string()).into_response()
}

async fn admin_get_endpoint_health(State(state): State<AppState>) -> Response {
    if let Some(ref tracker) = state.endpoint_health {
        Json(tracker.snapshot()).into_response()
    } else {
        Json(serde_json::json!({"endpoints": []})).into_response()
    }
}
