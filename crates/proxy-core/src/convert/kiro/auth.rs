//! Kiro authentication and token management.
//!
//! Handles OAuth token refresh for Social (Kiro Desktop) and IdC (AWS SSO OIDC) auth methods.
//! Supports multiple credentials with automatic failover.

use crate::config::KiroConfig;
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
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
        // Generate a stable machine ID using system fingerprint + token
        let machine_id = generate_machine_id(config.refresh_token.as_deref(), &region);
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

// ---- Token persistence ----

/// Serializable token record for file persistence.
#[derive(Serialize, Deserialize)]
struct TokenRecord {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<String>,
    profile_arn: Option<String>,
    region: String,
}

/// Save token to a JSON file for persistence across restarts.
fn persist_token(cred: &KiroCredential) {
    let path = format!(
        "{}/.config/model-proxy/kiro-token-{}.json",
        std::env::var("HOME").unwrap_or_else(|_| ".".to_string()),
        cred.region
    );

    let record = TokenRecord {
        access_token: cred.access_token.clone().unwrap_or_default(),
        refresh_token: cred.refresh_token.clone(),
        expires_at: cred.expires_at.map(|dt| dt.to_rfc3339()),
        profile_arn: cred.profile_arn.clone(),
        region: cred.region.clone(),
    };

    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match serde_json::to_string_pretty(&record) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(path = path.as_str(), error = %e, "保存 Kiro token 失败");
            } else {
                tracing::debug!(path = path.as_str(), "Kiro token 已保存");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "序列化 Kiro token 失败");
        }
    }
}

/// Try to load a persisted token from file.
fn load_persisted_token(region: &str) -> Option<TokenRecord> {
    let path = format!(
        "{}/.config/model-proxy/kiro-token-{}.json",
        std::env::var("HOME").unwrap_or_else(|_| ".".to_string()),
        region
    );
    let data = std::fs::read_to_string(&path).ok()?;
    let record: TokenRecord = serde_json::from_str(&data).ok()?;
    // Check if not expired
    if let Some(ref expires_str) = record.expires_at {
        if let Ok(expires) = chrono::DateTime::parse_from_rfc3339(expires_str) {
            if expires < Utc::now() + Duration::minutes(5) {
                return None; // Expired
            }
        }
    }
    Some(record)
}

// ---- Machine ID & System Fingerprint ----

/// Generate a per-credential machine ID using system fingerprint + token.
/// Priority: token hash + region > system hardware ID > random fallback.
fn generate_machine_id(refresh_token: Option<&str>, region: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    // Primary: refresh token + region
    if let Some(token) = refresh_token {
        token.hash(&mut hasher);
        region.hash(&mut hasher);
    } else {
        // Fallback: system hardware ID
        let hw_id = get_system_fingerprint();
        hw_id.hash(&mut hasher);
    }

    format!("{:016x}", hasher.finish())
}

/// Get a system-level hardware fingerprint for machine identification.
/// macOS: IOPlatformUUID, Linux: /etc/machine-id, Windows: wmic csproduct UUID
fn get_system_fingerprint() -> String {
    #[cfg(target_os = "macos")]
    {
        // Try IOPlatformUUID
        if let Ok(output) = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().find(|l| l.contains("IOPlatformUUID")) {
                if let Some(uuid) = line.split('"').nth(3) {
                    return uuid.to_string();
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
            return id.trim().to_string();
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = std::process::Command::new("wmic")
            .args(["csproduct", "get", "UUID"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(uuid) = stdout.lines().nth(1) {
                return uuid.trim().to_string();
            }
        }
    }

    // Fallback: username
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Try to auto-discover Profile ARN from Kiro IDE log files.
/// Scans ~/Library/Application Support/Kiro/logs/ (macOS) or equivalent.
pub fn discover_profile_arn() -> Option<String> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;

    // macOS Kiro log path
    #[cfg(target_os = "macos")]
    let log_dir = format!("{}/Library/Application Support/Kiro/logs", home);
    #[cfg(target_os = "linux")]
    let log_dir = format!("{}/.config/kiro/logs", home);
    #[cfg(target_os = "windows")]
    let log_dir = format!("{}\\AppData\\Roaming\\Kiro\\logs", home);

    let entries = std::fs::read_dir(&log_dir).ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "log").unwrap_or(false) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Look for profileArn in log content
                for line in content.lines() {
                    if line.contains("profileArn") || line.contains("profile_arn") {
                        // Extract ARN pattern
                        if let Some(start) = line.find("arn:aws:") {
                            let arn: String = line[start..]
                                .chars()
                                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                                .collect();
                            if arn.starts_with("arn:aws:codewhisperer:") {
                                return Some(arn);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Try to load credentials from kiro-cli's SQLite database.
/// Returns (refresh_token, region) if found.
pub fn load_from_sqlite() -> Option<(String, String)> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;

    // Platform-specific SQLite path
    #[cfg(target_os = "macos")]
    let db_path = format!("{}/.local/share/kiro-cli/data.sqlite3", home);
    #[cfg(target_os = "linux")]
    let db_path = format!("{}/.local/share/kiro-cli/data.sqlite3", home);
    #[cfg(target_os = "windows")]
    let db_path = format!("{}\\AppData\\Local\\kiro-cli\\data.sqlite3", home);

    // Try to read the SQLite file
    let data = std::fs::read(&db_path).ok()?;

    // Simple SQLite table scan for auth_kv entries
    // SQLite format: look for 'refreshToken' in the raw data
    let data_str = String::from_utf8_lossy(&data);

    // Look for social token key
    for key_prefix in &["kirocli:social:token", "kirocli:odic:token", "codewhisperer:odic:token"] {
        if let Some(pos) = data_str.find(key_prefix) {
            // Try to find a JSON object nearby containing refreshToken
            let nearby = &data_str[pos..std::cmp::min(pos + 2000, data_str.len())];
            if let Some(rt_start) = nearby.find("refreshToken") {
                let json_area = &nearby[rt_start..];
                // Simple extraction: find "refreshToken":"value"
                if let Some(q1) = json_area.find('"') {
                    let after_key = &json_area[q1 + 1..]; // skip first quote of key
                    if let Some(q2) = after_key.find('"') {
                        let after_colon = &after_key[q2 + 1..]; // skip closing quote of key
                        if let Some(q3) = after_colon.find('"') {
                            let value_start = q3 + 1;
                            if let Some(q4) = after_colon[value_start..].find('"') {
                                let token = &after_colon[value_start..value_start + q4];
                                if !token.is_empty() {
                                    tracing::info!(
                                        key = key_prefix,
                                        "从 kiro-cli SQLite 数据库加载 token"
                                    );
                                    return Some((token.to_string(), "us-east-1".to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Scan ~/.aws/sso/cache/ for Kiro credential JSON files.
pub fn scan_sso_cache() -> Vec<(String, String)> {
    let home = match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(h) => h,
        Err(_) => return vec![],
    };

    let cache_dir = format!("{}/.aws/sso/cache", home);
    let entries = match std::fs::read_dir(&cache_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut results = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Look for refreshToken field
                if content.contains("refreshToken") {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(token) = json.get("refreshToken").and_then(|v| v.as_str()) {
                            let region = json.get("region")
                                .or_else(|| json.get("ssoRegion"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("us-east-1")
                                .to_string();
                            results.push((token.to_string(), region));
                            tracing::info!(
                                path = %path.display(),
                                "从 SSO 缓存发现 Kiro 凭证"
                            );
                        }
                    }
                }
            }
        }
    }

    results
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
        Self::refresh_token(&mut self.credentials[idx], &self.client).await?;

        // Persist refreshed token
        persist_token(&self.credentials[idx]);

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
        Self::refresh_token(&mut self.credentials[idx], &self.client).await?;

        // Persist refreshed token
        persist_token(&self.credentials[idx]);

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
        };
        let cred = KiroCredential::from_config(&config);
        assert_eq!(cred.auth_method, AuthMethod::Social);
        assert_eq!(cred.region, "us-west-2");
        assert_eq!(cred.api_region, "us-west-2");
        assert!(!cred.is_api_key());
    }
}
