//! Site guard admin handlers (maintenance mode, self-use mode).

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::admin_success;
use crate::server::state::AppState;

// ---- Types ----

#[derive(Deserialize)]
pub struct ToggleRequest {
    pub enabled: bool,
}

#[derive(Serialize)]
pub(super) struct SiteStatusResponse {
    pub maintenance_mode: bool,
    pub self_use_mode: bool,
}

// ---- Handlers ----

pub(super) async fn admin_toggle_maintenance(
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

pub(super) async fn admin_toggle_self_use(
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

pub(super) async fn admin_get_site_status(
    State(state): State<AppState>,
) -> Json<SiteStatusResponse> {
    Json(SiteStatusResponse {
        maintenance_mode: state.site_guard.is_maintenance(),
        self_use_mode: state.site_guard.is_self_use(),
    })
}
