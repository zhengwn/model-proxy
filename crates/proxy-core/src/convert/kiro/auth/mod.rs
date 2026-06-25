//! Kiro authentication and token management.
//!
//! Handles OAuth token refresh for Social (Kiro Desktop) and IdC (AWS SSO OIDC) auth methods.
//! Supports multiple credentials with automatic failover.

use crate::config::KiroConfig;
use chrono::{Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

mod credential;
mod discovery;
mod fingerprint;
mod token_store;

pub use credential::{AuthMethod, CredentialSource, KiroCredential};
pub use discovery::{discover_profile_arn, load_from_sqlite, scan_sso_cache};
use token_store::{load_persisted_token, persist_token, write_back_to_source};


// ---- Refresh response ----

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    profile_arn: Option<String>,
}




// ---- Auth manager ----

/// Manages Kiro authentication tokens with automatic refresh.
pub struct KiroAuthManager {
    credentials: Vec<KiroCredential>,
    client: Client,
    /// Machine ID for User-Agent header
    machine_id: String,
    /// Kiro IDE version to impersonate
    kiro_version: String,
}

impl KiroAuthManager {
    /// Create a new auth manager from a KiroConfig.
    pub fn new(config: &KiroConfig, client: Client) -> Self {
        let cred = KiroCredential::from_config(config);
        let machine_id = cred.machine_id.clone();
        let kiro_version = config.kiro_version.clone().unwrap_or_else(|| "0.11.107".to_string());
        Self {
            credentials: vec![cred],
            client,
            machine_id,
            kiro_version,
        }
    }

    /// Get a valid access token, refreshing if necessary.
    pub async fn get_valid_token(&mut self) -> crate::error::Result<String> {
        // Find the first non-disabled credential index
        let idx = self
            .credentials
            .iter()
            .position(|c| !c.disabled)
            .ok_or_else(|| {
                crate::error::AppError::Request("所有 Kiro 凭证已禁用".to_string())
            })?;

        // API key credentials don't need refresh
        if self.credentials[idx].is_api_key() {
            return self.credentials[idx].access_token.clone().ok_or_else(|| {
                crate::error::AppError::Request("Kiro API Key 未配置".to_string())
            });
        }

        // Check if token is still valid
        if !self.credentials[idx].is_expired() {
            if let Some(token) = &self.credentials[idx].access_token {
                return Ok(token.clone());
            }
        }

        // Try loading persisted token before doing a network refresh
        if self.credentials[idx].access_token.is_none() {
            if let Some(record) = load_persisted_token(&self.credentials[idx].region) {
                info!("从持久化文件加载 Kiro token");
                self.credentials[idx].access_token = Some(record.access_token.clone());
                self.credentials[idx].refresh_token = record.refresh_token;
                self.credentials[idx].profile_arn = record.profile_arn;
                if !self.credentials[idx].is_expired() {
                    return Ok(record.access_token);
                }
            }
        }

        // Token is expired or missing — refresh it
        info!("Kiro token 已过期或缺失，正在刷新...");
        let original_rt = self.credentials[idx].refresh_token.clone().unwrap_or_default();
        let old_access_token = self.credentials[idx].access_token.clone();

        match Self::refresh_token(&mut self.credentials[idx], &self.client).await {
            Ok(()) => {
                // Success — persist and write back
                persist_token(&self.credentials[idx]);
                write_back_to_source(&self.credentials[idx], &original_rt);
            }
            Err(refresh_err) => {
                tracing::warn!(error = %refresh_err, "Kiro token 刷新失败，尝试降级策略");

                // Fallback 1: Try reloading from persisted file
                if let Some(record) = load_persisted_token(&self.credentials[idx].region) {
                    if !record.access_token.is_empty() && !record.access_token.starts_with("dummy") {
                        tracing::warn!("降级: 使用持久化文件中的 token");
                        self.credentials[idx].access_token = Some(record.access_token.clone());
                        self.credentials[idx].refresh_token = record.refresh_token;
                        self.credentials[idx].profile_arn = record.profile_arn;
                        if !self.credentials[idx].is_expired() {
                            return Ok(record.access_token);
                        }
                    }
                }

                // Fallback 2: Try reloading from SQLite and retry refresh
                if let Some((token, _region)) = load_from_sqlite() {
                    tracing::warn!("降级: 使用 SQLite 中的 refresh token 重新尝试");
                    self.credentials[idx].refresh_token = Some(token);
                    if Self::refresh_token(&mut self.credentials[idx], &self.client).await.is_ok() {
                        persist_token(&self.credentials[idx]);
                        return self.credentials[idx].access_token.clone().ok_or_else(|| {
                            crate::error::AppError::Request("Kiro token 刷新后仍为空".to_string())
                        });
                    }
                }

                // Fallback 3: Return old access token if it exists (with warning)
                if let Some(ref old_token) = old_access_token {
                    tracing::warn!("降级: 使用旧的 access_token（可能已过期）");
                    return Ok(old_token.clone());
                }

                // All fallbacks exhausted
                return Err(refresh_err);
            }
        }

        self.credentials[idx].access_token.clone().ok_or_else(|| {
            crate::error::AppError::Request("Kiro token 刷新后仍为空".to_string())
        })
    }

    /// Force refresh the token regardless of expiry status.
    /// Used when receiving 403 from the Kiro API.
    pub async fn force_refresh(&mut self) -> crate::error::Result<String> {
        let idx = self
            .credentials
            .iter()
            .position(|c| !c.disabled)
            .ok_or_else(|| {
                crate::error::AppError::Request("所有 Kiro 凭证已禁用".to_string())
            })?;

        if self.credentials[idx].is_api_key() {
            return self.credentials[idx].access_token.clone().ok_or_else(|| {
                crate::error::AppError::Request("Kiro API Key 未配置".to_string())
            });
        }

        info!("Kiro 强制刷新 token（403 触发）");
        let original_rt = self.credentials[idx].refresh_token.clone().unwrap_or_default();

        match Self::refresh_token(&mut self.credentials[idx], &self.client).await {
            Ok(()) => {
                persist_token(&self.credentials[idx]);
                write_back_to_source(&self.credentials[idx], &original_rt);
            }
            Err(refresh_err) => {
                tracing::warn!(error = %refresh_err, "强制刷新失败，尝试备用来源");

                // Fallback 1: Try persisted file
                if let Some(record) = load_persisted_token(&self.credentials[idx].region) {
                    if !record.access_token.is_empty() && !record.access_token.starts_with("dummy") {
                        self.credentials[idx].access_token = Some(record.access_token.clone());
                        self.credentials[idx].refresh_token = record.refresh_token;
                        if !self.credentials[idx].is_expired() {
                            tracing::warn!("降级: 使用持久化文件中的 token（强制刷新）");
                            persist_token(&self.credentials[idx]);
                            return Ok(record.access_token);
                        }
                    }
                }

                // Fallback 2: Try SQLite
                if let Some((token, _region)) = load_from_sqlite() {
                    self.credentials[idx].refresh_token = Some(token);
                    if Self::refresh_token(&mut self.credentials[idx], &self.client).await.is_ok() {
                        persist_token(&self.credentials[idx]);
                        return self.credentials[idx].access_token.clone().ok_or_else(|| {
                            crate::error::AppError::Request("Kiro token 强制刷新后仍为空".to_string())
                        });
                    }
                }

                return Err(refresh_err);
            }
        }

        self.credentials[idx].access_token.clone().ok_or_else(|| {
            crate::error::AppError::Request("Kiro token 强制刷新后仍为空".to_string())
        })
    }

    /// Refresh the token for a credential.
    pub async fn refresh_token(cred: &mut KiroCredential, client: &Client) -> crate::error::Result<()> {
        let refresh_token = cred.refresh_token.clone().ok_or_else(|| {
            crate::error::AppError::Request("Kiro refresh token 未配置".to_string())
        })?;

        match cred.auth_method {
            AuthMethod::Social => Self::refresh_social(client, cred, &refresh_token).await,
            AuthMethod::IdC => Self::refresh_idc(client, cred, &refresh_token).await,
            AuthMethod::ApiKey => {
                // API key doesn't need refresh
                Ok(())
            }
        }
    }

    async fn refresh_social(
        client: &Client,
        cred: &mut KiroCredential,
        refresh_token: &str,
    ) -> crate::error::Result<()> {
        let url = format!(
            "https://prod.{}.auth.desktop.kiro.dev/refreshToken",
            cred.region
        );

        info!(region = cred.region.as_str(), "刷新 Kiro Social token");

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header(
                "User-Agent",
                format!("KiroIDE-{}-{}", "0.11.107", cred.machine_id),
            )
            .header("Accept", "application/json, text/plain, */*")
            .json(&serde_json::json!({"refreshToken": refresh_token}))
            .send()
            .await
            .map_err(crate::error::AppError::Http)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();

            if status == 400 && body.contains("invalid_grant") {
                error!("Kiro refresh token 已永久失效");
                cred.disabled = true;
                return Err(crate::error::AppError::Request(
                    "Kiro refresh token 已失效，需要重新认证".to_string(),
                ));
            }

            warn!(status, body = body.as_str(), "Kiro token 刷新失败");
            return Err(crate::error::AppError::Request(format!(
                "Kiro token 刷新失败: HTTP {}",
                status
            )));
        }

        let data: RefreshResponse = resp
            .json()
            .await
            .map_err(crate::error::AppError::Http)?;

        cred.access_token = Some(data.access_token);
        if let Some(new_token) = data.refresh_token {
            cred.refresh_token = Some(new_token);
        }
        if let Some(arn) = data.profile_arn {
            cred.profile_arn = Some(arn);
        }
        if let Some(expires_in) = data.expires_in {
            cred.expires_at = Some(Utc::now() + Duration::seconds(expires_in));
        }

        info!("Kiro Social token 刷新成功");
        Ok(())
    }

    async fn refresh_idc(
        client: &Client,
        cred: &mut KiroCredential,
        refresh_token: &str,
    ) -> crate::error::Result<()> {
        let url = format!("https://oidc.{}.amazonaws.com/token", cred.region);
        let client_id = cred.client_id.as_ref().ok_or_else(|| {
            crate::error::AppError::Request("Kiro IdC client_id 未配置".to_string())
        })?;
        let client_secret = cred.client_secret.as_ref().ok_or_else(|| {
            crate::error::AppError::Request("Kiro IdC client_secret 未配置".to_string())
        })?;

        info!(region = cred.region.as_str(), "刷新 Kiro IdC token");

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header(
                "x-amz-user-agent",
                "aws-sdk-js/3.980.0 KiroIDE".to_string(),
            )
            .header(
                "user-agent",
                format!(
                    "aws-sdk-js/3.980.0 ua/2.1 os/{} lang/js md/nodejs#22.0.0 api/sso-oidc#3.980.0 m/E KiroIDE",
                    std::env::consts::OS
                ),
            )
            .json(&serde_json::json!({
                "clientId": client_id,
                "clientSecret": client_secret,
                "refreshToken": refresh_token,
                "grantType": "refresh_token"
            }))
            .send()
            .await
            .map_err(crate::error::AppError::Http)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();

            if status == 400 && body.contains("invalid_grant") {
                error!("Kiro IdC refresh token 已永久失效");
                cred.disabled = true;
                return Err(crate::error::AppError::Request(
                    "Kiro IdC refresh token 已失效，需要重新认证".to_string(),
                ));
            }

            warn!(status, body = body.as_str(), "Kiro IdC token 刷新失败");
            return Err(crate::error::AppError::Request(format!(
                "Kiro IdC token 刷新失败: HTTP {}",
                status
            )));
        }

        let data: RefreshResponse = resp
            .json()
            .await
            .map_err(crate::error::AppError::Http)?;

        cred.access_token = Some(data.access_token);
        if let Some(new_token) = data.refresh_token {
            cred.refresh_token = Some(new_token);
        }
        if let Some(arn) = data.profile_arn {
            cred.profile_arn = Some(arn);
        }
        if let Some(expires_in) = data.expires_in {
            cred.expires_at = Some(Utc::now() + Duration::seconds(expires_in));
        }

        info!("Kiro IdC token 刷新成功");
        Ok(())
    }

    /// Build the User-Agent header for Kiro API requests.
    pub fn user_agent(&self) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#22.0.0 api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            std::env::consts::OS,
            self.kiro_version,
            self.machine_id
        )
    }

    /// Build the x-amz-user-agent header.
    pub fn amz_user_agent(&self) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            self.kiro_version, self.machine_id
        )
    }

    /// Get the profile ARN from the first non-disabled credential.
    pub fn profile_arn(&self) -> Option<&str> {
        self.credentials
            .iter()
            .find(|c| !c.disabled)
            .and_then(|c| c.profile_arn.as_deref())
    }

    /// Iterate over all credentials.
    pub fn credentials_iter(&self) -> impl Iterator<Item = &KiroCredential> {
        self.credentials.iter()
    }
}

/// Thread-safe wrapper for KiroAuthManager.
pub type SharedKiroAuth = Arc<Mutex<KiroAuthManager>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_method_from_str() {
        assert_eq!(AuthMethod::from_str("social"), AuthMethod::Social);
        assert_eq!(AuthMethod::from_str("idc"), AuthMethod::IdC);
        assert_eq!(AuthMethod::from_str("builder-id"), AuthMethod::IdC);
        assert_eq!(AuthMethod::from_str("iam"), AuthMethod::IdC);
        assert_eq!(AuthMethod::from_str("api_key"), AuthMethod::ApiKey);
        assert_eq!(AuthMethod::from_str("apikey"), AuthMethod::ApiKey);
    }

    #[test]
    fn credential_expiry() {
        let mut cred = KiroCredential {
            auth_method: AuthMethod::Social,
            access_token: None,
            refresh_token: None,
            client_id: None,
            client_secret: None,
            profile_arn: None,
            expires_at: None,
            region: "us-east-1".to_string(),
            api_region: "us-east-1".to_string(),
            machine_id: "test".to_string(),
            disabled: false,
            source: CredentialSource::Unknown,
        };

        // No expiry = expired
        assert!(cred.is_expired());

        // Set expiry to 1 minute from now = expired (within 5 min buffer)
        cred.expires_at = Some(Utc::now() + Duration::minutes(1));
        assert!(cred.is_expired());

        // Set expiry to 30 minutes from now = not expired
        cred.expires_at = Some(Utc::now() + Duration::minutes(30));
        assert!(!cred.is_expired());
        assert!(!cred.is_expiring_soon());

        // Set expiry to 8 minutes from now = expiring soon but not expired
        cred.expires_at = Some(Utc::now() + Duration::minutes(8));
        assert!(!cred.is_expired());
        assert!(cred.is_expiring_soon());
    }

    #[test]
    fn credential_from_config() {
        let config = KiroConfig {
            auth_method: "social".to_string(),
            refresh_token: Some("test-token".to_string()),
            client_id: None,
            client_secret: None,
            profile_arn: None,
            region: "us-west-2".to_string(),
            api_region: Some("us-west-2".to_string()),
            model_aliases: None,
            hidden_models: None,
            kiro_version: None,
            proxy_url: None,
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
        let cred = KiroCredential::from_config(&config);
        assert_eq!(cred.auth_method, AuthMethod::Social);
        assert_eq!(cred.region, "us-west-2");
        assert_eq!(cred.api_region, "us-west-2");
        assert!(!cred.is_api_key());
    }
}
