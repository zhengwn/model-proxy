//! Tauri IPC commands for Kiro credential and settings management.
//!
//! These commands are thin wrappers that proxy to the running proxy server's
//! admin HTTP API (`/api/admin/*`). The shared `admin_get`/`admin_post` helpers
//! handle authentication and error mapping.

use tauri::State;

use super::{get_config_internal, sanitize_credential_id, TauriState};

/// Build the admin API base URL and get the API key from config.
/// Acquires config_lock to ensure consistent read.
async fn admin_api_base(state: &TauriState) -> Result<(String, String), String> {
    let _lock = state.config_lock.lock().await;
    let config = get_config_internal(&state.config_path)?;
    let port = config.server.port;
    let api_key = config.server.admin_api_key.clone().unwrap_or_default();
    let base = format!("http://127.0.0.1:{}", port);
    Ok((base, api_key))
}

/// Helper to make an authenticated GET request to the admin API.
async fn admin_get(state: &TauriState, path: &str) -> Result<serde_json::Value, String> {
    let (base, api_key) = admin_api_base(state).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .connect_timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| format!("创建客户端失败: {}", e))?;
    let mut req = client.get(format!("{}/api/admin/{}", base, path));
    if !api_key.is_empty() {
        req = req.header("x-api-key", &api_key);
    }
    let resp = req.send().await.map_err(|e| format!("请求失败: {}", e))?;
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("解析响应失败: {}", e))?;
    if status >= 400 {
        let msg = body["error"]["message"].as_str().unwrap_or("未知错误");
        return Err(format!("API 错误 ({}): {}", status, msg));
    }
    Ok(body)
}

/// Helper to make an authenticated POST request to the admin API.
async fn admin_post(state: &TauriState, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let (base, api_key) = admin_api_base(state).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .connect_timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| format!("创建客户端失败: {}", e))?;
    let mut req = client.post(format!("{}/api/admin/{}", base, path))
        .header("Content-Type", "application/json")
        .json(&body);
    if !api_key.is_empty() {
        req = req.header("x-api-key", &api_key);
    }
    let resp = req.send().await.map_err(|e| format!("请求失败: {}", e))?;
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("解析响应失败: {}", e))?;
    if status >= 400 {
        let msg = body["error"]["message"].as_str().unwrap_or("未知错误");
        return Err(format!("API 错误 ({}): {}", status, msg));
    }
    Ok(body)
}

/// List all Kiro credentials.
#[tauri::command]
pub async fn kiro_list_credentials(state: State<'_, TauriState>) -> Result<serde_json::Value, String> {
    admin_get(&state, "credentials").await
}

/// Add a new Kiro credential.
#[tauri::command]
pub async fn kiro_add_credential(
    state: State<'_, TauriState>,
    refresh_token: String,
    auth_method: Option<String>,
    region: Option<String>,
    priority: Option<u32>,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({
        "refresh_token": refresh_token,
        "auth_method": auth_method.unwrap_or_else(|| "social".to_string()),
        "region": region.unwrap_or_else(|| "us-east-1".to_string()),
        "priority": priority.unwrap_or(0),
    });
    admin_post(&state, "credentials", body).await
}

/// Delete a Kiro credential.
#[tauri::command]
pub async fn kiro_delete_credential(state: State<'_, TauriState>, id: String) -> Result<serde_json::Value, String> {
    let id = sanitize_credential_id(&id)?;
    let (base, api_key) = admin_api_base(&state).await?;
    let client = reqwest::Client::new();
    let mut req = client.delete(format!("{}/api/admin/credentials/{}", base, id));
    if !api_key.is_empty() {
        req = req.header("x-api-key", &api_key);
    }
    let resp = req.send().await.map_err(|e| format!("请求失败: {}", e))?;
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("解析响应失败: {}", e))?;
    if status >= 400 {
        let msg = body["error"]["message"].as_str().unwrap_or("未知错误");
        return Err(format!("API 错误 ({}): {}", status, msg));
    }
    Ok(body)
}

/// Enable/disable a Kiro credential.
#[tauri::command]
pub async fn kiro_set_credential_disabled(
    state: State<'_, TauriState>,
    id: String,
    disabled: bool,
) -> Result<serde_json::Value, String> {
    let id = sanitize_credential_id(&id)?;
    admin_post(&state, &format!("credentials/{}/disabled", id), serde_json::json!({ "disabled": disabled })).await
}

/// Batch operations on Kiro credentials.
#[tauri::command]
pub async fn kiro_batch_credentials(
    state: State<'_, TauriState>,
    ids: Vec<String>,
    action: String,
) -> Result<serde_json::Value, String> {
    admin_post(&state, "credentials/batch", serde_json::json!({ "ids": ids, "action": action })).await
}

/// Test a Kiro credential.
#[tauri::command]
pub async fn kiro_test_credential(state: State<'_, TauriState>, id: String) -> Result<serde_json::Value, String> {
    let id = sanitize_credential_id(&id)?;
    admin_post(&state, &format!("credentials/{}/test", id), serde_json::json!({})).await
}

/// Get full details of a Kiro credential.
#[tauri::command]
pub async fn kiro_get_credential_full(state: State<'_, TauriState>, id: String) -> Result<serde_json::Value, String> {
    let id = sanitize_credential_id(&id)?;
    admin_get(&state, &format!("credentials/{}/full", id)).await
}

/// Force refresh a Kiro credential token.
#[tauri::command]
pub async fn kiro_refresh_credential(state: State<'_, TauriState>, id: String) -> Result<serde_json::Value, String> {
    let id = sanitize_credential_id(&id)?;
    admin_post(&state, &format!("credentials/{}/refresh", id), serde_json::json!({})).await
}

/// Reset failure count for a Kiro credential.
#[tauri::command]
pub async fn kiro_reset_credential(state: State<'_, TauriState>, id: String) -> Result<serde_json::Value, String> {
    let id = sanitize_credential_id(&id)?;
    admin_post(&state, &format!("credentials/{}/reset", id), serde_json::json!({})).await
}

/// Get endpoint health data.
#[tauri::command]
pub async fn kiro_get_endpoint_health(state: State<'_, TauriState>) -> Result<serde_json::Value, String> {
    admin_get(&state, "endpoints/health").await
}

/// Get thinking config.
#[tauri::command]
pub async fn kiro_get_thinking(state: State<'_, TauriState>) -> Result<serde_json::Value, String> {
    admin_get(&state, "thinking").await
}

/// Set thinking config.
#[tauri::command]
pub async fn kiro_set_thinking(state: State<'_, TauriState>, mode: String) -> Result<serde_json::Value, String> {
    admin_post(&state, "thinking", serde_json::json!({ "mode": mode })).await
}

/// Get proxy settings.
#[tauri::command]
pub async fn kiro_get_settings(state: State<'_, TauriState>) -> Result<serde_json::Value, String> {
    admin_get(&state, "settings").await
}

/// Set proxy settings.
#[tauri::command]
pub async fn kiro_set_settings(
    state: State<'_, TauriState>,
    preferred_endpoint: Option<String>,
    endpoint_fallback: Option<bool>,
) -> Result<serde_json::Value, String> {
    let mut body = serde_json::json!({});
    if let Some(ep) = preferred_endpoint {
        body["preferred_endpoint"] = serde_json::json!(ep);
    }
    if let Some(fb) = endpoint_fallback {
        body["endpoint_fallback"] = serde_json::json!(fb);
    }
    admin_post(&state, "settings", body).await
}

/// Start IAM IdC SSO login flow.
#[tauri::command]
pub async fn kiro_start_iam_sso(
    state: State<'_, TauriState>,
    start_url: String,
    region: String,
) -> Result<serde_json::Value, String> {
    admin_post(&state, "kiro/auth/iam-sso/start", serde_json::json!({
        "start_url": start_url,
        "region": region,
    })).await
}

/// Complete IAM IdC SSO login flow.
#[tauri::command]
pub async fn kiro_complete_iam_sso(
    state: State<'_, TauriState>,
    session_id: String,
    callback_url: String,
) -> Result<serde_json::Value, String> {
    admin_post(&state, "kiro/auth/iam-sso/complete", serde_json::json!({
        "session_id": session_id,
        "callback_url": callback_url,
    })).await
}

/// Import SSO tokens (batch, newline-separated).
#[tauri::command]
pub async fn kiro_import_sso_tokens(
    state: State<'_, TauriState>,
    tokens: String,
    region: Option<String>,
) -> Result<serde_json::Value, String> {
    admin_post(&state, "kiro/auth/sso-token", serde_json::json!({
        "tokens": tokens,
        "region": region.unwrap_or_else(|| "us-east-1".to_string()),
    })).await
}

/// Get load balancing config.
#[tauri::command]
pub async fn kiro_get_lb_config(state: State<'_, TauriState>) -> Result<serde_json::Value, String> {
    admin_get(&state, "config").await
}

/// Set load balancing mode.
#[tauri::command]
pub async fn kiro_set_lb_config(state: State<'_, TauriState>, mode: String) -> Result<serde_json::Value, String> {
    admin_post(&state, "config", serde_json::json!({ "mode": mode })).await
}
