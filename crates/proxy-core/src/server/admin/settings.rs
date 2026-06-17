//! Admin config, thinking, settings, and endpoint health handlers.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::admin_error;
use super::admin_success;
use crate::convert::kiro::account::LoadBalancingMode;
use crate::server::state::AppState;
use crate::ProviderRegistry;

// ---- Response types ----

#[derive(Serialize)]
pub(super) struct AdminConfigResponse {
    pub load_balancing_mode: String,
    pub account_count: usize,
    pub rate_limiter_enabled: bool,
    pub flow_monitor_enabled: bool,
}

#[derive(Deserialize)]
pub struct SetConfigRequest {
    pub load_balancing_mode: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ThinkingConfigResponse {
    pub mode: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ProxySettingsResponse {
    pub preferred_endpoint: Option<String>,
    pub endpoint_fallback: Option<bool>,
}

fn store_updated_active_provider(state: &AppState, provider: crate::config::ProviderConfig) {
    let provider_name = provider.name.clone();
    state.active_provider.store(Arc::new(provider.clone()));

    let mut providers = state.registry.load().list().to_vec();
    if let Some(existing) = providers.iter_mut().find(|p| p.name == provider_name) {
        *existing = provider;
        if let Ok(registry) = ProviderRegistry::new(providers) {
            state.registry.store(Arc::new(registry));
        }
    }
}

// ---- Handlers ----

pub(super) async fn admin_get_config(
    State(state): State<AppState>,
) -> Json<AdminConfigResponse> {
    let mode = if let Some(mgr) = state.kiro_account_manager() {
        let mgr = mgr.lock().await;
        mgr.load_balancing_mode().as_str().to_string()
    } else {
        "single".to_string()
    };

    let account_count = if let Some(mgr) = state.kiro_account_manager() {
        let mgr = mgr.lock().await;
        mgr.account_count()
    } else {
        1
    };

    Json(AdminConfigResponse {
        load_balancing_mode: mode,
        account_count,
        rate_limiter_enabled: state.rate_limiter().is_some(),
        flow_monitor_enabled: state.flow_monitor().is_some(),
    })
}

pub(super) async fn admin_set_config(
    State(state): State<AppState>,
    Json(req): Json<SetConfigRequest>,
) -> Response {
    if let Some(mode_str) = req.load_balancing_mode {
        if let Some(mgr_arc) = state.kiro_account_manager() {
            let mode = LoadBalancingMode::from_str(&mode_str);
            let mut mgr = mgr_arc.lock().await;
            mgr.set_load_balancing_mode(mode.clone());
            Json(AdminConfigResponse {
                load_balancing_mode: mode.as_str().to_string(),
                account_count: mgr.account_count(),
                rate_limiter_enabled: state.rate_limiter().is_some(),
                flow_monitor_enabled: state.flow_monitor().is_some(),
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

pub(super) async fn admin_get_thinking(State(state): State<AppState>) -> Response {
    let provider = state.current_provider();
    let mode = provider
        .kiro_config
        .as_ref()
        .and_then(|c| c.thinking_mode.clone())
        .unwrap_or_else(|| "as_reasoning_content".to_string());

    Json(ThinkingConfigResponse { mode }).into_response()
}

pub(super) async fn admin_set_thinking(
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

        store_updated_active_provider(&state, new_provider);
    }

    admin_success(format!("Thinking mode set to '{}'", req.mode)).into_response()
}

pub(super) async fn admin_get_settings(State(state): State<AppState>) -> Response {
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

pub(super) async fn admin_set_settings(
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

        store_updated_active_provider(&state, new_provider);
    }

    admin_success("Settings updated".to_string()).into_response()
}

pub(super) async fn admin_get_endpoint_health(State(state): State<AppState>) -> Response {
    if let Some(tracker) = state.endpoint_health() {
        Json(tracker.snapshot()).into_response()
    } else {
        Json(serde_json::json!({"endpoints": []})).into_response()
    }
}
