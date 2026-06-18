//! Credential management admin handlers.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde::Serialize;
use tracing::error;

use super::{admin_error, admin_success};
use crate::config::KiroConfig;
use crate::server::state::AppState;

// ---- Response types ----

#[derive(Serialize)]
pub(super) struct CredentialsStatusResponse {
    pub total: usize,
    pub available: usize,
    pub credentials: Vec<CredentialSnapshotResponse>,
}

#[derive(Serialize)]
pub(super) struct CredentialSnapshotResponse {
    pub id: String,
    pub priority: u32,
    pub disabled: bool,
    pub failure_count: u32,
    pub is_current: bool,
    pub is_available: bool,
    pub auth_method: String,
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub proxy_url: Option<String>,
    pub region: String,
    pub health_score: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_exhausted: Option<bool>,
}

#[derive(Serialize)]
#[allow(dead_code)] // Documents the balance API response schema; handler currently passes through raw JSON.
pub(super) struct BalanceResponse {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_title: Option<String>,
    pub current_usage: f64,
    pub usage_limit: f64,
    pub remaining: f64,
    pub usage_percentage: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset_at: Option<f64>,
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
pub(super) struct BatchCredentialRequest {
    pub ids: Vec<String>,
    pub action: String, // "enable" | "disable" | "refresh"
}

// ---- Handlers ----

pub(super) async fn admin_list_credentials(
    State(state): State<AppState>,
) -> Response {
    let Some(mgr) = state.kiro_account_manager() else {
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
            quota_remaining: s.quota_remaining,
            quota_limit: s.quota_limit,
            quota_exhausted: s.quota_exhausted,
        })
        .collect();

    Json(CredentialsStatusResponse {
        total,
        available,
        credentials,
    })
    .into_response()
}

pub(super) async fn admin_add_credential(
    State(state): State<AppState>,
    Json(req): Json<AddCredentialRequest>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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
        filter_env_noise: None,
        filter_strip_boundaries: None,
        first_token_timeout: None,
        streaming_read_timeout: None,
        first_token_max_retries: None,
        quota_cooldown_secs: None,
        health_score_decay: None,
        health_score_recovery: None,
        preferred_endpoint: None,
        endpoint_fallback: None,
        debug_save_requests: None,
        smart_summary_enabled: None,
        enable_quota_check: None,
        quota_check_interval_secs: None,
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

pub(super) async fn admin_delete_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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

pub(super) async fn admin_set_disabled(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetDisabledRequest>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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

pub(super) async fn admin_set_priority(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetPriorityRequest>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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

pub(super) async fn admin_reset_failures(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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

pub(super) async fn admin_force_refresh(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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
        Err(e) => {
            error!(credential_id = %id, error = %e, "Token refresh failed");
            admin_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Token refresh failed",
            )
        }
    }
}

pub(super) async fn admin_get_balance(
    State(state): State<AppState>,
    Path(_id): Path<String>,
) -> Response {
    // Balance query requires an auth token; use the account manager's auth
    let Some(mgr_arc) = state.kiro_account_manager() else {
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
                        error!(error = %e, "Failed to get auth token for balance query");
                        return admin_error(
                            StatusCode::BAD_GATEWAY,
                            "api_error",
                            "Failed to get auth token",
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

    // Find the active Kiro config for region info.
    let provider = state.current_provider();
    let kiro_config = provider.kiro_config.as_ref();

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
            error!(status = %status, body = %text, "Upstream balance query failed");
            admin_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("Upstream error (HTTP {})", status),
            )
        }
        Err(e) => admin_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            format!("Network error: {}", e),
        ),
    }
}

pub(super) async fn admin_batch_credentials(
    State(state): State<AppState>,
    Json(req): Json<BatchCredentialRequest>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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

pub(super) async fn admin_test_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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

pub(super) async fn admin_get_credential_full(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(mgr_arc) = state.kiro_account_manager() else {
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
