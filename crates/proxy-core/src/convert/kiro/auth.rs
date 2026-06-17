//! Kiro authentication and token management.
//!
//! Handles OAuth token refresh for Social (Kiro Desktop) and IdC (AWS SSO OIDC) auth methods.
//! Supports multiple credentials with automatic failover.

use crate::config::KiroConfig;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
    #[allow(clippy::should_implement_trait)]
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

/// Where a credential was originally loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// From TOML config file (KiroConfig / KiroAccountEntry)
    Config,
    /// From model-proxy's own persisted JSON cache
    PersistedFile,
    /// From kiro-cli SQLite database
    KiroCliSqlite,
    /// From AWS SSO cache
    AwsSsoCache,
    /// Unknown or ad-hoc
    Unknown,
}

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
    /// Where this credential was originally loaded from
    pub source: CredentialSource,
}

impl KiroCredential {
    /// Create a credential from KiroConfig.
    pub fn from_config(config: &KiroConfig) -> Self {
        let region = config.region.clone();
        let api_region = config.api_region.clone().unwrap_or_else(|| region.clone());
        // Generate a stable machine ID using system fingerprint + token
        let machine_id = generate_machine_id(config.refresh_token.as_deref(), &region);
        let auth_method = AuthMethod::from_str(&config.auth_method);
        let access_token = if auth_method == AuthMethod::ApiKey {
            config.refresh_token.clone()
        } else {
            None
        };
        let refresh_token = if auth_method == AuthMethod::ApiKey {
            None
        } else {
            config.refresh_token.clone()
        };
        Self {
            auth_method,
            access_token,
            refresh_token,
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            profile_arn: config.profile_arn.clone(),
            expires_at: None,
            region,
            api_region,
            machine_id,
            disabled: false,
            source: CredentialSource::Config,
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
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let path = format!(
        "{}/.config/model-proxy/kiro-token-{}.json",
        home,
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
        restrict_dir_permissions(parent);
    }

    match serde_json::to_string_pretty(&record) {
        Ok(json) => {
            if let Err(e) = write_secret_file(&path, json.as_bytes()) {
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

/// Write a file containing secrets with owner-only permissions (0600 on Unix).
///
/// On Unix the file is created with mode 0600 before any data is written, so the
/// secret is never briefly world-readable. On other platforms it falls back to a
/// regular write (NTFS ACLs already restrict to the user profile directory).
fn write_secret_file(path: &str, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // Ensure mode is 0600 even if the file already existed with looser bits.
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = file.set_permissions(perms);
        file.write_all(contents)
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// Restrict a directory holding secrets to owner-only access (0700 on Unix).
fn restrict_dir_permissions(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Write refreshed token back to the original credential source.
/// This ensures other clients (kiro-cli, kiro IDE) can use the updated token.
fn write_back_to_source(cred: &KiroCredential, original_refresh_token: &str) {
    // Only write back if the refresh token actually changed
    let new_token = match &cred.refresh_token {
        Some(t) if t != original_refresh_token => t,
        _ => return,
    };

    match cred.source {
        CredentialSource::KiroCliSqlite => {
            write_back_to_sqlite(cred, new_token);
        }
        CredentialSource::AwsSsoCache => {
            // SSO cache tokens are managed by AWS SSO, don't write back
            tracing::debug!("SSO cache token refreshed, not writing back (managed by AWS SSO)");
        }
        CredentialSource::Config | CredentialSource::PersistedFile | CredentialSource::Unknown => {
            // Already handled by persist_token() which writes to model-proxy's cache
            tracing::debug!(
                source = ?cred.source,
                "Token persisted to model-proxy cache"
            );
        }
    }
}

/// Escape a string for safe embedding as a single-quoted SQLite string literal.
///
/// Within a SQLite single-quoted string literal the *only* metacharacter is the
/// single quote, which is escaped by doubling it (`''`). Backslashes and double
/// quotes have **no** special meaning in SQLite string literals and must NOT be
/// pre-escaped — doing so would corrupt the stored value. This is the same
/// transformation a prepared-statement text binding performs internally, so it
/// is injection-safe.
fn sqlite_quote_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Try to write refreshed token back to kiro-cli SQLite database.
fn write_back_to_sqlite(cred: &KiroCredential, new_refresh_token: &str) {
    use std::io::Write;

    let home = match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(h) => h,
        Err(_) => return,
    };

    #[cfg(target_os = "macos")]
    let db_path = format!("{}/.local/share/kiro-cli/data.sqlite3", home);
    #[cfg(target_os = "linux")]
    let db_path = format!("{}/.local/share/kiro-cli/data.sqlite3", home);
    #[cfg(target_os = "windows")]
    let db_path = format!("{}\\AppData\\Local\\kiro-cli\\data.sqlite3", home);

    if !std::path::Path::new(&db_path).exists() {
        return;
    }

    // Defense in depth: treat the refresh token as untrusted (it comes from a
    // network response). A legitimate OAuth refresh token never contains NUL or
    // other control characters and is not pathologically long. Refuse to write
    // back anything that looks malformed, so a compromised upstream cannot use
    // the token to tamper with the user's local kiro-cli database.
    if new_refresh_token.is_empty()
        || new_refresh_token.len() > 8192
        || new_refresh_token.chars().any(|c| c.is_control())
    {
        tracing::warn!("refresh token 含非法字符或长度异常，跳过 SQLite 回写");
        return;
    }

    // Validate and prepare additional fields to write back atomically.
    let access_token_literal = cred.access_token.as_ref()
        .filter(|t| !t.is_empty() && t.len() <= 8192 && !t.chars().any(|c| c.is_control()))
        .map(|t| sqlite_quote_literal(t));

    let profile_arn_literal = cred.profile_arn.as_ref()
        .filter(|a| !a.is_empty() && a.len() <= 2048 && !a.chars().any(|c| c.is_control()))
        .map(|a| sqlite_quote_literal(a));

    // Build a chained json_set() SQL to update refreshToken + accessToken + profileArn atomically.
    let token_literal = sqlite_quote_literal(new_refresh_token);

    // SQLite json_set() supports multiple path-value pairs in a single call:
    //   json_set(value, '$.refreshToken', '<rt>', '$.accessToken', '<at>', ...)
    let mut paths = format!("'$.refreshToken', '{}'", token_literal);
    if let Some(ref at) = access_token_literal {
        paths.push_str(&format!(", '$.accessToken', '{}'", at));
    }
    if let Some(ref arn) = profile_arn_literal {
        paths.push_str(&format!(", '$.profileArn', '{}'", arn));
    }

    // Note: key_prefix values below are hardcoded constants (not user input),
    // so they are safe to embed directly.
    for key_prefix in &["kirocli:social:token", "kirocli:oidc:token"] {
        let sql = format!(
            "UPDATE auth_kv SET value = json_set(value, {}) WHERE key = '{}';",
            paths, key_prefix
        );

        // Feed the SQL (which contains the secret) via stdin rather than argv so
        // the refresh token does not appear in the process argument list, which
        // is readable by other local users (e.g. via `ps`).
        let mut child = match std::process::Command::new("sqlite3")
            .arg(&db_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "sqlite3 CLI 不可用，跳过 kiro-cli token 回写"
                );
                return;
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(sql.as_bytes()) {
                tracing::debug!(error = %e, "写入 sqlite3 stdin 失败");
                let _ = child.kill();
                let _ = child.wait();
                continue;
            }
            // stdin dropped here → EOF signalled to sqlite3
        }

        match child.wait_with_output() {
            Ok(output) if output.status.success() => {
                tracing::info!(
                    db_path = db_path.as_str(),
                    key = key_prefix,
                    "已将刷新后的 token 回写到 kiro-cli SQLite"
                );
                return;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::debug!(
                    key = key_prefix,
                    stderr = %stderr,
                    "SQLite 回写失败 (尝试下一个 key)"
                );
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "等待 sqlite3 进程失败"
                );
            }
        }
    }
}

/// Try to load a persisted token from file.
fn load_persisted_token(region: &str) -> Option<TokenRecord> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let path = format!(
        "{}/.config/model-proxy/kiro-token-{}.json",
        home,
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
    for key_prefix in &["kirocli:social:token", "kirocli:oidc:token", "codewhisperer:oidc:token"] {
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
    fn sqlite_quote_literal_doubles_single_quotes() {
        // Single quotes are the only metacharacter; they are doubled.
        assert_eq!(sqlite_quote_literal("abc"), "abc");
        assert_eq!(sqlite_quote_literal("a'b"), "a''b");
        assert_eq!(sqlite_quote_literal("''"), "''''");
    }

    #[test]
    fn sqlite_quote_literal_leaves_backslash_and_quotes_untouched() {
        // Backslashes and double quotes have no special meaning in SQLite
        // string literals and must NOT be escaped (escaping would corrupt them).
        assert_eq!(sqlite_quote_literal("a\\b"), "a\\b");
        assert_eq!(sqlite_quote_literal("a\"b"), "a\"b");
    }

    #[test]
    fn sqlite_quote_literal_neutralizes_injection_attempt() {
        // A malicious token attempting to break out of the string literal is
        // rendered inert: every single quote is doubled, so when embedded in
        // '{}' the value cannot terminate the literal early.
        let malicious = "x'); DROP TABLE auth_kv;--";
        let quoted = sqlite_quote_literal(malicious);
        assert_eq!(quoted, "x''); DROP TABLE auth_kv;--");

        // Security invariant: in the escaped output every single quote belongs
        // to a doubled pair, i.e. there is no lone quote that could close the
        // surrounding literal. Verify by confirming each run of consecutive
        // quotes has even length.
        let mut run = 0usize;
        for ch in quoted.chars() {
            if ch == '\'' {
                run += 1;
            } else {
                assert_eq!(run % 2, 0, "found an odd run of single quotes");
                run = 0;
            }
        }
        assert_eq!(run % 2, 0, "trailing odd run of single quotes");
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
            first_token_timeout: None,
            streaming_read_timeout: None,
            first_token_max_retries: None,
            quota_cooldown_secs: None,
            health_score_decay: None,
            health_score_recovery: None,
            preferred_endpoint: None,
            endpoint_fallback: None,
        };
        let cred = KiroCredential::from_config(&config);
        assert_eq!(cred.auth_method, AuthMethod::Social);
        assert_eq!(cred.region, "us-west-2");
        assert_eq!(cred.api_region, "us-west-2");
        assert!(!cred.is_api_key());
    }
}
