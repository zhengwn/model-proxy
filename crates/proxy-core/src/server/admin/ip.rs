//! IP ban/unban admin handlers.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

use super::{admin_error, admin_success, AdminSuccess};
use crate::server::state::AppState;

// ---- Types ----

#[derive(Deserialize)]
pub struct IpActionRequest {
    pub ip: IpAddr,
}

#[derive(Serialize)]
pub(super) struct IpListResponse {
    pub banned: Vec<String>,
}

// ---- Handlers ----

pub(super) async fn admin_ip_ban(
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

pub(super) async fn admin_ip_unban(
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

pub(super) async fn admin_ip_list(
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
