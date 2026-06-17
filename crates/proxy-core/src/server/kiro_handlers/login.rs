//! Kiro login/social OAuth route handlers (device flow + social OAuth).

use axum::{
    body::Body,
    http::{header, StatusCode},
    Json,
};
use serde_json::{json, Value};

use crate::error::{AppError, Result};

/// Handle `POST /api/kiro/login/start` — start OIDC Device Authorization Flow.
pub async fn proxy_kiro_login_start(
    axum::extract::State(state): axum::extract::State<crate::server::state::AppState>,
) -> Result<axum::response::Response> {
    use crate::convert::kiro::auth_flow::start_device_flow;

    let provider = state.current_provider();
    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    let region = &kiro_config.region;
    let result = start_device_flow(&state.client, region).await?;

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&json!({
            "user_code": result.user_code,
            "verification_uri": result.verification_uri,
            "verification_uri_complete": result.verification_uri_complete,
            "expires_in": result.expires_in,
            "interval": result.interval.unwrap_or(5),
        })).unwrap_or_default()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))
}

/// Handle `POST /api/kiro/login/poll` — poll OIDC device flow for token.
pub async fn proxy_kiro_login_poll(
    axum::extract::State(state): axum::extract::State<crate::server::state::AppState>,
    Json(body): Json<Value>,
) -> Result<axum::response::Response> {
    use crate::convert::kiro::auth_flow::poll_device_token;

    let provider = state.current_provider();
    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    let client_id = body.get("client_id").and_then(|v| v.as_str()).ok_or_else(|| {
        AppError::Request("缺少 client_id".to_string())
    })?;
    let device_code = body.get("device_code").and_then(|v| v.as_str()).ok_or_else(|| {
        AppError::Request("缺少 device_code".to_string())
    })?;

    let status = poll_device_token(&state.client, &kiro_config.region, client_id, device_code).await;

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&status).unwrap_or_default()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))
}

/// Handle `POST /api/kiro/social/start` — start Social OAuth flow.
pub async fn proxy_kiro_social_start(
    Json(body): Json<Value>,
) -> Result<axum::response::Response> {
    use crate::convert::kiro::auth_flow::{start_social_auth, OAuthProvider};

    let provider_str = body.get("provider").and_then(|v| v.as_str()).unwrap_or("google");
    let redirect_uri = body.get("redirect_uri").and_then(|v| v.as_str())
        .unwrap_or("http://localhost:19823/callback");
    let state_param = body.get("state").and_then(|v| v.as_str()).unwrap_or("kiro_login");

    let oauth_provider = match provider_str {
        "google" => OAuthProvider::Google,
        "github" => OAuthProvider::GitHub,
        _ => return Err(AppError::Request(format!("不支持的 OAuth provider: {}", provider_str))),
    };

    match start_social_auth(oauth_provider, redirect_uri, state_param) {
        Ok(url) => Ok(axum::response::Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"auth_url": url}).to_string()))
            .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?),
        Err(e) => Err(e),
    }
}

/// Handle `POST /api/kiro/social/exchange` — exchange OAuth code for Kiro token.
pub async fn proxy_kiro_social_exchange(
    axum::extract::State(state): axum::extract::State<crate::server::state::AppState>,
    Json(body): Json<Value>,
) -> Result<axum::response::Response> {
    use crate::convert::kiro::auth_flow::{exchange_social_code, exchange_for_kiro_token, OAuthProvider};

    let provider_str = body.get("provider").and_then(|v| v.as_str()).unwrap_or("google");
    let code = body.get("code").and_then(|v| v.as_str()).ok_or_else(|| {
        AppError::Request("缺少 code".to_string())
    })?;

    let oauth_provider = match provider_str {
        "google" => OAuthProvider::Google,
        "github" => OAuthProvider::GitHub,
        _ => return Err(AppError::Request(format!("不支持的 OAuth provider: {}", provider_str))),
    };

    let redirect_uri = body.get("redirect_uri").and_then(|v| v.as_str())
        .unwrap_or("http://localhost:19823/callback");

    // Exchange OAuth code for social token
    let social_token = exchange_social_code(&state.client, oauth_provider, code, redirect_uri).await?;
    let social_access = social_token.access_token.ok_or_else(|| {
        AppError::Request("OAuth token 交换失败: 无 access_token".to_string())
    })?;

    // Exchange social token for Kiro token
    let provider = state.current_provider();
    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    let (access_token, refresh_token, expires_in) =
        exchange_for_kiro_token(&state.client, &social_access, oauth_provider, &kiro_config.region).await?;

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({
            "access_token": access_token,
            "refresh_token": refresh_token,
            "expires_in": expires_in,
        }).to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))
}
