use tauri::State;
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

use super::{get_config_internal, persist_config, TauriState};
use crate::service::ServiceManager;
use proxy_core::config::{KiroConfig, ProviderFormat, KiroAccountEntry};
use proxy_core::ProviderRegistry;
use proxy_core::convert::kiro::auth_flow;

#[derive(Serialize)]
pub struct KiroCredentialStatus {
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
}

async fn update_active_kiro_config<F>(state: &TauriState, update: F) -> Result<(), String>
where
    F: FnOnce(&mut KiroConfig),
{
    let _lock = state.config_lock.lock().await;
    let mut config = get_config_internal(&state.config_path)?;
    config.normalize();

    let active_name = config
        .active_provider
        .clone()
        .or_else(|| config.providers.first().map(|p| p.name.clone()))
        .ok_or_else(|| "没有可更新的 Provider".to_string())?;

    let provider = config
        .providers
        .iter_mut()
        .find(|p| p.name == active_name)
        .ok_or_else(|| format!("Provider 未找到: {}", active_name))?;

    if provider.format != ProviderFormat::Kiro {
        return Err(format!("当前活跃 Provider '{}' 不是 Kiro", provider.name));
    }

    let kiro_config = provider
        .kiro_config
        .as_mut()
        .ok_or_else(|| format!("Kiro Provider '{}' 缺少 kiro_config", provider.name))?;
    
    update(kiro_config);

    persist_config(&state.config_path, &config)?;

    let new_registry = ProviderRegistry::new(config.providers.clone())
        .map_err(|e| format!("构建 Registry 失败: {}", e))?;
    state.registry.store(Arc::new(new_registry));

    if let Some(updated) = config.providers.iter().find(|p| p.name == active_name) {
        state.active_provider.store(Arc::new(updated.clone()));
    }

    Ok(())
}

/// List all Kiro credentials.
#[tauri::command]
pub async fn kiro_list_credentials(state: State<'_, TauriState>, service: State<'_, ServiceManager>) -> Result<serde_json::Value, String> {
    // Read config under config_lock, then release it before accessing service state
    let accounts = {
        let _lock = state.config_lock.lock().await;
        let config = get_config_internal(&state.config_path)?;
        let kiro_config = config.providers.iter()
            .find(|p| p.format == ProviderFormat::Kiro)
            .and_then(|p| p.kiro_config.as_ref());
        kiro_config.and_then(|c| c.accounts.clone()).unwrap_or_default()
    }; // config_lock released here

    let mut results = Vec::new();
    
    // Check if service is running to fetch live stats (no config_lock needed)
    let live_stats = if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mgr = mgr_arc.lock().await;
                Some(mgr.snapshot())
            } else { None }
        } else { None }
    } else { None };

    for (i, acc) in accounts.into_iter().enumerate() {
        let id = acc.id.clone().unwrap_or_else(|| format!("kiro-{}", i));
        
        let mut failure_count = 0;
        let mut health_score = 100;
        let mut total_requests = 0;
        let mut successful_requests = 0;
        let mut failed_requests = 0;
        let mut is_available = true;
        
        if let Some(stats) = &live_stats {
            if let Some(stat) = stats.iter().find(|s| s.id == id) {
                failure_count = stat.failure_count;
                health_score = stat.health_score;
                total_requests = stat.total_requests;
                successful_requests = stat.successful_requests;
                failed_requests = stat.failed_requests;
                is_available = stat.is_available;
            }
        }
        
        results.push(KiroCredentialStatus {
            id,
            priority: acc.priority.unwrap_or(0),
            disabled: acc.disabled.unwrap_or(false),
            failure_count,
            is_current: false, // Could be derived if needed
            is_available,
            auth_method: acc.auth_method.unwrap_or_else(|| "social".to_string()),
            total_requests,
            successful_requests,
            failed_requests,
            proxy_url: acc.proxy_url.clone(),
            region: acc.region.unwrap_or_else(|| "us-east-1".to_string()),
            health_score,
        });
    }
    
    Ok(serde_json::json!({ "credentials": results }))
}

/// Add a new Kiro credential.
#[tauri::command]
pub async fn kiro_add_credential(
    state: State<'_, TauriState>,
    service: State<'_, ServiceManager>,
    refresh_token: String,
    auth_method: Option<String>,
    region: Option<String>,
    priority: Option<u32>,
) -> Result<serde_json::Value, String> {
    let id = Uuid::new_v4().to_string();
    let entry = KiroAccountEntry {
        id: Some(id.clone()),
        auth_method: Some(auth_method.unwrap_or_else(|| "social".to_string())),
        refresh_token: Some(refresh_token.clone()),
        region: Some(region.unwrap_or_else(|| "us-east-1".to_string())),
        priority,
        client_id: None,
        client_secret: None,
        profile_arn: None,
        api_region: None,
        proxy_url: None,
        disabled: Some(false),
    };

    let mut full_cfg = None;
    
    update_active_kiro_config(&state, |kiro_config| {
        let mut accounts = kiro_config.accounts.clone().unwrap_or_default();
        accounts.push(entry.clone());
        kiro_config.accounts = Some(accounts);
        full_cfg = Some(kiro_config.clone());
    }).await?;
    
    // If proxy is running, inject it dynamically
    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mut mgr = mgr_arc.lock().await;
                // Construct a KiroConfig for this specific account
                let base = full_cfg.unwrap_or_default();
                let acc_cfg = KiroConfig {
                    auth_method: entry.auth_method.unwrap_or(base.auth_method),
                    refresh_token: entry.refresh_token,
                    region: entry.region.unwrap_or(base.region),
                    ..base
                };
                mgr.add_account(id.clone(), &acc_cfg, app_state.client.clone(), entry.priority.unwrap_or(0));
                if entry.disabled.unwrap_or(false) {
                    mgr.set_disabled(&id, true);
                }
            }
        }
    }

    Ok(serde_json::json!({
        "success": true,
        "message": "Credential added",
        "credentialId": id,
    }))
}

/// Delete a Kiro credential.
#[tauri::command]
pub async fn kiro_delete_credential(state: State<'_, TauriState>, service: State<'_, ServiceManager>, id: String) -> Result<serde_json::Value, String> {
    update_active_kiro_config(&state, |kiro_config| {
        if let Some(mut accounts) = kiro_config.accounts.clone() {
            accounts.retain(|a| a.id.as_deref() != Some(id.as_str()));
            kiro_config.accounts = Some(accounts);
        }
    }).await?;

    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mut mgr = mgr_arc.lock().await;
                mgr.remove_account(&id);
            }
        }
    }

    Ok(serde_json::json!({ "success": true, "message": "Credential deleted" }))
}

/// Enable/disable a Kiro credential.
#[tauri::command]
pub async fn kiro_set_credential_disabled(
    state: State<'_, TauriState>,
    service: State<'_, ServiceManager>,
    id: String,
    disabled: bool,
) -> Result<serde_json::Value, String> {
    update_active_kiro_config(&state, |kiro_config| {
        if let Some(mut accounts) = kiro_config.accounts.clone() {
            for a in &mut accounts {
                if a.id.as_deref() == Some(id.as_str()) {
                    a.disabled = Some(disabled);
                }
            }
            kiro_config.accounts = Some(accounts);
        }
    }).await?;

    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mut mgr = mgr_arc.lock().await;
                mgr.set_disabled(&id, disabled);
            }
        }
    }

    Ok(serde_json::json!({ "success": true }))
}

/// Batch operations on Kiro credentials.
#[tauri::command]
pub async fn kiro_batch_credentials(
    state: State<'_, TauriState>,
    service: State<'_, ServiceManager>,
    ids: Vec<String>,
    action: String,
) -> Result<serde_json::Value, String> {
    let mut updated = false;
    update_active_kiro_config(&state, |kiro_config| {
        if let Some(mut accounts) = kiro_config.accounts.clone() {
            if action == "delete" {
                accounts.retain(|a| !ids.contains(&a.id.clone().unwrap_or_default()));
                updated = true;
            } else if action == "disable" || action == "enable" {
                let disabled = action == "disable";
                for a in &mut accounts {
                    if ids.contains(&a.id.clone().unwrap_or_default()) {
                        a.disabled = Some(disabled);
                        updated = true;
                    }
                }
            }
            kiro_config.accounts = Some(accounts);
        }
    }).await?;

    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mut mgr = mgr_arc.lock().await;
                if action == "delete" {
                    for id in ids {
                        mgr.remove_account(&id);
                    }
                } else if action == "disable" || action == "enable" {
                    let disabled = action == "disable";
                    for id in ids {
                        mgr.set_disabled(&id, disabled);
                    }
                }
            }
        }
    }

    Ok(serde_json::json!({ "success": true, "updated": updated }))
}

/// Reset failure count for a Kiro credential.
#[tauri::command]
pub async fn kiro_reset_credential(_state: State<'_, TauriState>, service: State<'_, ServiceManager>, id: String) -> Result<serde_json::Value, String> {
    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mut mgr = mgr_arc.lock().await;
                mgr.reset_failures(&id);
            }
        }
    }
    Ok(serde_json::json!({ "success": true }))
}

/// Start IAM IdC SSO login flow.
#[tauri::command]
pub async fn kiro_start_iam_sso(
    start_url: String,
    region: String,
) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::new();
    let resp = auth_flow::start_iam_sso_login(&client, &start_url, &region).await
        .map_err(|e| format!("Start SSO failed: {}", e))?;
    
    Ok(serde_json::to_value(resp).unwrap())
}

/// Complete IAM IdC SSO login flow.
#[tauri::command]
pub async fn kiro_complete_iam_sso(
    session_id: String,
    callback_url: String,
) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::new();
    let resp = auth_flow::complete_iam_sso_login(&client, &session_id, &callback_url).await
        .map_err(|e| format!("Complete SSO failed: {}", e))?;
    
    Ok(serde_json::to_value(resp).unwrap())
}


/// Get full details of a Kiro credential.
#[tauri::command]
pub async fn kiro_get_credential_full(state: State<'_, TauriState>, service: State<'_, ServiceManager>, id: String) -> Result<serde_json::Value, String> {
    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mgr = mgr_arc.lock().await;
                if let Some(snapshot) = mgr.account_full_snapshot(&id) {
                    return Ok(snapshot);
                }
            }
        }
    }
    
    // Fallback to static config
    let _lock = state.config_lock.lock().await;
    let config = get_config_internal(&state.config_path)?;
    let kiro_config = config.providers.iter()
        .find(|p| p.format == ProviderFormat::Kiro)
        .and_then(|p| p.kiro_config.as_ref());
    
    let accounts = kiro_config.and_then(|c| c.accounts.clone()).unwrap_or_default();
    if let Some(acc) = accounts.into_iter().find(|a| a.id.as_deref() == Some(id.as_str())) {
        return Ok(serde_json::json!({
            "id": acc.id,
            "priority": acc.priority,
            "disabled": acc.disabled,
            "proxy_url": acc.proxy_url,
            "credentials": {
                "auth_method": acc.auth_method,
                "region": acc.region,
                "api_region": acc.api_region,
            }
        }));
    }
    
    Err("Credential not found".to_string())
}

/// Force refresh a Kiro credential token.
#[tauri::command]
pub async fn kiro_refresh_credential(_state: State<'_, TauriState>, service: State<'_, ServiceManager>, id: String) -> Result<serde_json::Value, String> {
    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mgr = mgr_arc.lock().await;
                match mgr.force_refresh_account(&id).await {
                    Ok(_) => return Ok(serde_json::json!({ "success": true })),
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Err("Service not running. Cannot refresh token.".to_string())
}

/// Get endpoint health data.
#[tauri::command]
pub async fn kiro_get_endpoint_health(service: State<'_, ServiceManager>) -> Result<serde_json::Value, String> {
    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            let tracker = kiro.endpoint_health.snapshot();
            let mut healths = Vec::new();
            for ep in tracker.endpoints {
                healths.push(serde_json::json!({
                    "name": ep.endpoint,
                    "health_score": (ep.success_rate * 100.0) as u32,
                    "failure_count": ep.fail_count,
                    "state": if ep.consecutive_errors > 3 { "Broken" } else { "Active" },
                }));
            }
            return Ok(serde_json::json!({ "endpoints": healths }));
        }
    }
    Ok(serde_json::json!({ "endpoints": [] }))
}

/// Get thinking config.
#[tauri::command]
pub async fn kiro_get_thinking(state: State<'_, TauriState>) -> Result<serde_json::Value, String> {
    let _lock = state.config_lock.lock().await;
    let config = get_config_internal(&state.config_path)?;
    let kiro_config = config.providers.iter()
        .find(|p| p.format == ProviderFormat::Kiro)
        .and_then(|p| p.kiro_config.as_ref());
    
    let mode = kiro_config.and_then(|c| c.thinking_mode.clone()).unwrap_or_else(|| "auto".to_string());
    Ok(serde_json::json!({ "mode": mode }))
}

/// Set thinking config.
#[tauri::command]
pub async fn kiro_set_thinking(state: State<'_, TauriState>, mode: String) -> Result<serde_json::Value, String> {
    update_active_kiro_config(&state, |config| {
        config.thinking_mode = Some(mode);
    }).await?;
    Ok(serde_json::json!({ "success": true }))
}

/// Get proxy settings.
#[tauri::command]
pub async fn kiro_get_settings(state: State<'_, TauriState>) -> Result<serde_json::Value, String> {
    let _lock = state.config_lock.lock().await;
    let config = get_config_internal(&state.config_path)?;
    let kiro_config = config.providers.iter()
        .find(|p| p.format == ProviderFormat::Kiro)
        .and_then(|p| p.kiro_config.as_ref());
    
    Ok(serde_json::json!({
        "preferred_endpoint": kiro_config.and_then(|c| c.preferred_endpoint.clone()),
        "endpoint_fallback": kiro_config.and_then(|c| c.endpoint_fallback).unwrap_or(true),
    }))
}

/// Set proxy settings.
#[tauri::command]
pub async fn kiro_set_settings(
    state: State<'_, TauriState>,
    preferred_endpoint: Option<String>,
    endpoint_fallback: Option<bool>,
) -> Result<serde_json::Value, String> {
    update_active_kiro_config(&state, |config| {
        if let Some(ep) = preferred_endpoint {
            config.preferred_endpoint = Some(ep);
        }
        if let Some(fb) = endpoint_fallback {
            config.endpoint_fallback = Some(fb);
        }
    }).await?;
    Ok(serde_json::json!({ "success": true }))
}

/// Get load balancing config.
#[tauri::command]
pub async fn kiro_get_lb_config(state: State<'_, TauriState>) -> Result<serde_json::Value, String> {
    let _lock = state.config_lock.lock().await;
    let config = get_config_internal(&state.config_path)?;
    let kiro_config = config.providers.iter()
        .find(|p| p.format == ProviderFormat::Kiro)
        .and_then(|p| p.kiro_config.as_ref());
    
    let mode = kiro_config.and_then(|c| c.load_balancing_mode.clone()).unwrap_or_else(|| "priority".to_string());
    Ok(serde_json::json!({ "mode": mode }))
}

/// Set load balancing mode.
#[tauri::command]
pub async fn kiro_set_lb_config(state: State<'_, TauriState>, service: State<'_, ServiceManager>, mode: String) -> Result<serde_json::Value, String> {
    update_active_kiro_config(&state, |config| {
        config.load_balancing_mode = Some(mode.clone());
    }).await?;

    if let Some(app_state) = service.get_app_state().await {
        if let Some(kiro) = &app_state.kiro {
            if let Some(mgr_arc) = &kiro.account_manager {
                let mut mgr = mgr_arc.lock().await;
                let m = match mode.as_str() {
                    "priority" => proxy_core::convert::kiro::account::LoadBalancingMode::Priority,
                    "balanced" => proxy_core::convert::kiro::account::LoadBalancingMode::Balanced,
                    "smart" => proxy_core::convert::kiro::account::LoadBalancingMode::Smart,
                    _ => proxy_core::convert::kiro::account::LoadBalancingMode::Priority,
                };
                mgr.set_load_balancing_mode(m);
            }
        }
    }

    Ok(serde_json::json!({ "success": true }))
}

/// Import SSO tokens (batch, newline-separated).
#[tauri::command]
pub async fn kiro_import_sso_tokens(
    _state: State<'_, TauriState>,
    _tokens: String,
    _region: Option<String>,
) -> Result<serde_json::Value, String> {
    Err("Not implemented yet".to_string())
}

/// Test a Kiro credential.
#[tauri::command]
pub async fn kiro_test_credential(_state: State<'_, TauriState>, _service: State<'_, ServiceManager>, _id: String) -> Result<serde_json::Value, String> {
    Err("Not implemented yet".to_string())
}
