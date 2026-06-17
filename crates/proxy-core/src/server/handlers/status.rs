//! Telemetry intake and status/usage/flows endpoints.

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::debug;

use crate::convert::utils::truncate_for_log;
use crate::server::state::MAX_LOG_BODY_BYTES;
use crate::error::{AppError, Result};

pub async fn event_logging_batch(body: String) -> Json<Value> {
    let body_len = body.len();
    let body_preview = truncate_for_log(&body, MAX_LOG_BODY_BYTES);
    let event_count = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.as_array().map(|arr| arr.len()))
        .unwrap_or(0);
    debug!(
        event_count,
        body_bytes = body_len,
        body = %body_preview,
        "收到遥测事件"
    );
    Json(json!({"status": "ok"}))
}

/// Handle `GET /api/usage` — query Kiro usage/balance information.
pub async fn proxy_usage(
    State(state): State<crate::server::state::AppState>,
) -> Result<Response> {
    let provider = state.current_provider();
    if provider.format != crate::config::ProviderFormat::Kiro {
        return Err(AppError::Request("Usage 查询仅支持 Kiro provider".to_string()));
    }

    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    // Get auth token
    let token = if let Some(auth_arc) = state.kiro_auth() {
        let mut auth = auth_arc.lock().await;
        auth.get_valid_token().await.unwrap_or_default()
    } else {
        return Err(AppError::Request("Kiro auth 未初始化".to_string()));
    };

    let region = kiro_config.api_region.as_deref().unwrap_or(&kiro_config.region);
    let url = format!("https://q.{}.amazonaws.com/getUsageLimits", region);

    let resp = state.client
        .post(&url)
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("Authorization", format!("Bearer {}", token))
        .header("x-amz-target", "AmazonCodeWhispererStreamingService.GetUsageLimits")
        .timeout(Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body = r.text().await.unwrap_or_default();
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
        }
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            Ok((
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({"error": {"message": body, "type": "upstream_error"}})),
            ).into_response())
        }
        Err(e) => Err(AppError::Http(e)),
    }
}

/// Handle `GET /api/flows` — query flow monitor data.
pub async fn proxy_flows(
    State(state): State<crate::server::state::AppState>,
) -> Result<Response> {
    if let Some(monitor) = state.flow_monitor() {
        let monitor = monitor.lock().await;
        let stats = monitor.get_stats();
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&stats).unwrap_or_default()))
            .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
    } else {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":"flow monitor not enabled"}"#.to_string()))
            .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
    }
}

/// Handle `GET /api/status` — service status.
pub async fn proxy_status(
    State(state): State<crate::server::state::AppState>,
) -> Result<Response> {
    let provider = state.current_provider();
    let kiro_auth_ok = state.kiro_auth().is_some();

    let status = json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "provider": provider.name,
        "format": format!("{:?}", provider.format),
        "kiro_auth": kiro_auth_ok,
        "account_manager": state.kiro_account_manager().is_some(),
        "flow_monitor": state.flow_monitor().is_some(),
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(status.to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))
}
