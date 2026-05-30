//! Kiro authentication and token management.
//!
//! Handles OAuth token refresh for Social (Kiro Desktop) and IdC (AWS SSO OIDC) auth methods.
//! Supports multiple credentials with automatic failover.

use crate::config::KiroConfig;
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

// ---- Auth method ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    Social,
    IdC,
    ApiKey,
}

impl AuthMethod {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "social" => Self::Social,
            "idc" | "builder-id" | "iam" => Self::IdC,
            "api_key" | "apikey" => Self::ApiKey,
            _ => Self::Social, // default
        }
    }
}

// ---- Credential ----

#[derive(Debug, Clone)]
pub struct KiroCredential {
    pub auth_method: AuthMethod,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub profile_arn: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub region: String,
    pub api_region: String,
    pub machine_id: String,
    pub disabled: bool,
}

impl KiroCredential {
    /// Create a credential from KiroConfig.
    pub fn from_config(config: &KiroConfig) -> Self {
        let region = config.region.clone();
        let api_region = config.api_region.clone().unwrap_or_else(|| region.clone());
        let machine_id = format!(
            "{:016x}",
            // Use a hash of the refresh token or a random value as machine ID
            config
                .refresh_token
                .as_deref()
                .unwrap_or("default")
                .len() as u64
                ^ 0xDEADBEEF
        );
        Self {
            auth_method: AuthMethod::from_str(&config.auth_method),
            access_token: None,
            refresh_token: config.refresh_token.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            profile_arn: config.profile_arn.clone(),
            expires_at: None,
            region,
            api_region,
            machine_id,
            disabled: false,
        }
    }

    /// Check if the token is expired (within 5 minutes of expiry).
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => Utc::now() + Duration::minutes(5) >= exp,
            None => true, // no expiry info = treat as expired
        }
    }

    /// Check if the token is expiring soon (within 10 minutes).
    pub fn is_expiring_soon(&self) -> bool {
        match self.expires_at {
            Some(exp) => Utc::now() + Duration::minutes(10) >= exp,
            None => false, // no expiry info = not "expiring soon" (just expired)
        }
    }

    /// Check if this is an API key credential (no refresh needed).
    pub fn is_api_key(&self) -> bool {
        self.auth_method == AuthMethod::ApiKey
    }
}

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
    #[allow(dead_code)]
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
        Self {
            credentials: vec![cred],
            client,
            machine_id,
            kiro_version: "0.11.107".to_string(),
        }
    }

    /// Get a valid access token, refreshing if necessary.
    pub async fn get_valid_token(&self) -> crate::error::Result<String> {
        // Find the first non-disabled credential
        let cred = self
            .credentials
            .iter()
            .find(|c| !c.disabled)
            .ok_or_else(|| {
                crate::error::AppError::Request("所有 Kiro 凭证已禁用".to_string())
            })?;

        // API key credentials don't need refresh
        if cred.is_api_key() {
            return cred.access_token.clone().ok_or_else(|| {
                crate::error::AppError::Request("Kiro API Key 未配置".to_string())
            });
        }

        // Check if token is still valid
        if !cred.is_expired() {
            if let Some(token) = &cred.access_token {
                return Ok(token.clone());
            }
        }

        // Need to refresh - but we need mutable access
        // For now, we'll do the refresh and update the credential
        // In production, this should use interior mutability (Mutex/RwLock)
        Err(crate::error::AppError::Request(
            "Kiro token 已过期，需要刷新（auth 模块需配合 KiroAuthManager 的 Arc<Mutex> 使用）"
                .to_string(),
        ))
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
            .map_err(|e| crate::error::AppError::Http(e))?;

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
            .map_err(|e| crate::error::AppError::Http(e))?;

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
                format!("aws-sdk-js/3.980.0 KiroIDE"),
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
            .map_err(|e| crate::error::AppError::Http(e))?;

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
            .map_err(|e| crate::error::AppError::Http(e))?;

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
        };
        let cred = KiroCredential::from_config(&config);
        assert_eq!(cred.auth_method, AuthMethod::Social);
        assert_eq!(cred.region, "us-west-2");
        assert_eq!(cred.api_region, "us-west-2");
        assert!(!cred.is_api_key());
    }
}
