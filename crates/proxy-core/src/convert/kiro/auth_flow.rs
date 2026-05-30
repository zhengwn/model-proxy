//! Kiro authentication flows.
//!
//! Implements OIDC Device Authorization Flow and Social OAuth (Google/GitHub PKCE)
//! for initial credential acquisition.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::error::{AppError, Result};

// ---- OIDC Device Authorization Flow ----

/// Response from OIDC device authorization endpoint.
#[derive(Debug, Deserialize)]
pub struct DeviceAuthorizationResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: Option<u64>,
}

/// Response from OIDC token endpoint during device flow polling.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DeviceTokenResponse {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub expires_in: Option<i64>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Status of a device flow login attempt.
#[derive(Debug, Clone, Serialize)]
pub enum DeviceFlowStatus {
    /// Waiting for user to enter the code
    Pending {
        user_code: String,
        verification_uri: String,
        verification_uri_complete: Option<String>,
        expires_in: u64,
    },
    /// User authorized, tokens received
    Completed {
        access_token: String,
        refresh_token: String,
        expires_in: Option<i64>,
    },
    /// Still waiting for authorization
    AuthorizationPending,
    /// User denied authorization
    Denied,
    /// Code expired
    Expired,
    /// Error occurred
    Error(String),
}

/// Start OIDC Device Authorization Flow.
///
/// 1. Register a client at `oidc.{region}.amazonaws.com/client/register`
/// 2. Request device code at `oidc.{region}.amazonaws.com/device_authorization`
/// 3. Return the user code and verification URI for the user to enter
pub async fn start_device_flow(
    client: &Client,
    region: &str,
) -> Result<DeviceAuthorizationResponse> {
    let oidc_base = format!("https://oidc.{}.amazonaws.com", region);

    // Step 1: Register client
    let register_resp = client
        .post(format!("{}/client/register", oidc_base))
        .header("Content-Type", "application/json")
        .json(&json!({
            "clientName": format!("KiroProxy-{}", &uuid_simple()[..8]),
            "clientType": "public",
            "scopes": ["codewhisperer:completions", "codewhisperer:analysis"]
        }))
        .send()
        .await
        .map_err(|e| AppError::Http(e))?;

    if !register_resp.status().is_success() {
        let status = register_resp.status().as_u16();
        let body = register_resp.text().await.unwrap_or_default();
        error!(status, body = body.as_str(), "OIDC 客户端注册失败");
        return Err(AppError::Request(format!("OIDC 注册失败: HTTP {}", status)));
    }

    let client_reg: Value = register_resp.json().await.map_err(|e| AppError::Http(e))?;
    let client_id = client_reg.get("client_id").and_then(|v| v.as_str()).ok_or_else(|| {
        AppError::Request("OIDC 注册响应缺少 client_id".to_string())
    })?;

    info!(client_id, "OIDC 客户端注册成功");

    // Step 2: Request device authorization
    let device_resp = client
        .post(format!("{}/device_authorization", oidc_base))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "client_id={}&scope=codewhisperer:completions+codewhisperer:analysis",
            client_id
        ))
        .send()
        .await
        .map_err(|e| AppError::Http(e))?;

    if !device_resp.status().is_success() {
        let status = device_resp.status().as_u16();
        let body = device_resp.text().await.unwrap_or_default();
        return Err(AppError::Request(format!("Device authorization 失败: HTTP {}", status)));
    }

    let auth_resp: DeviceAuthorizationResponse =
        device_resp.json().await.map_err(|e| AppError::Http(e))?;

    info!(
        user_code = auth_resp.user_code.as_str(),
        verification_uri = auth_resp.verification_uri.as_str(),
        "OIDC 设备授权已启动"
    );

    Ok(auth_resp)
}

/// Poll for device flow token.
///
/// Call this repeatedly until the user authorizes or the code expires.
/// The recommended polling interval is 5 seconds.
pub async fn poll_device_token(
    client: &Client,
    region: &str,
    client_id: &str,
    device_code: &str,
) -> DeviceFlowStatus {
    let oidc_base = format!("https://oidc.{}.amazonaws.com", region);

    let resp = match client
        .post(format!("{}/token", oidc_base))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=urn:ietf:params:oauth:grant-type:device_code&client_id={}&device_code={}",
            client_id, device_code
        ))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return DeviceFlowStatus::Error(e.to_string()),
    };

    let token_resp: DeviceTokenResponse = match resp.json().await {
        Ok(r) => r,
        Err(e) => return DeviceFlowStatus::Error(e.to_string()),
    };

    if let Some(error) = &token_resp.error {
        match error.as_str() {
            "authorization_pending" => DeviceFlowStatus::AuthorizationPending,
            "slow_down" => DeviceFlowStatus::AuthorizationPending,
            "access_denied" => DeviceFlowStatus::Denied,
            "expired_token" => DeviceFlowStatus::Expired,
            _ => DeviceFlowStatus::Error(
                token_resp.error_description.clone().unwrap_or_else(|| error.clone())
            ),
        }
    } else if let (Some(access), Some(refresh)) = (&token_resp.access_token, &token_resp.refresh_token) {
        DeviceFlowStatus::Completed {
            access_token: access.clone(),
            refresh_token: refresh.clone(),
            expires_in: token_resp.expires_in,
        }
    } else {
        DeviceFlowStatus::Error("Unexpected token response".to_string())
    }
}

// ---- Social OAuth (Google/GitHub) ----

/// OAuth provider type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OAuthProvider {
    Google,
    GitHub,
}

impl OAuthProvider {
    fn auth_url(&self) -> &str {
        match self {
            OAuthProvider::Google => "https://accounts.google.com/o/oauth2/v2/auth",
            OAuthProvider::GitHub => "https://github.com/login/oauth/authorize",
        }
    }

    fn token_url(&self) -> &str {
        match self {
            OAuthProvider::Google => "https://oauth2.googleapis.com/token",
            OAuthProvider::GitHub => "https://github.com/login/oauth/access_token",
        }
    }

    fn client_id_env(&self) -> &str {
        match self {
            OAuthProvider::Google => "KIRO_GOOGLE_CLIENT_ID",
            OAuthProvider::GitHub => "KIRO_GITHUB_CLIENT_ID",
        }
    }

    fn client_secret_env(&self) -> &str {
        match self {
            OAuthProvider::Google => "KIRO_GOOGLE_CLIENT_SECRET",
            OAuthProvider::GitHub => "KIRO_GITHUB_CLIENT_SECRET",
        }
    }
}

/// Start Social OAuth flow.
/// Returns the authorization URL the user should visit.
pub fn start_social_auth(
    provider: OAuthProvider,
    redirect_uri: &str,
    state: &str,
) -> Result<String> {
    let client_id = std::env::var(provider.client_id_env()).map_err(|_| {
        AppError::Request(format!("{} 未配置", provider.client_id_env()))
    })?;

    let scopes = match provider {
        OAuthProvider::Google => "openid email profile",
        OAuthProvider::GitHub => "read:user",
    };

    let url = format!(
        "{}?client_id={}&redirect_uri={}&scope={}&state={}&response_type=code&code_challenge_method=S256",
        provider.auth_url(),
        client_id,
        percent_encode(redirect_uri),
        percent_encode(scopes),
        state
    );

    Ok(url)
}

/// Simple percent-encoding for URLs.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Exchange OAuth code for tokens.
pub async fn exchange_social_code(
    client: &Client,
    provider: OAuthProvider,
    code: &str,
    redirect_uri: &str,
) -> Result<SocialTokenResponse> {
    let client_id = std::env::var(provider.client_id_env()).map_err(|_| {
        AppError::Request(format!("{} 未配置", provider.client_id_env()))
    })?;
    let client_secret = std::env::var(provider.client_secret_env()).map_err(|_| {
        AppError::Request(format!("{} 未配置", provider.client_secret_env()))
    })?;

    let mut req = client.post(provider.token_url())
        .header("Accept", "application/json");

    let body = match provider {
        OAuthProvider::Google => json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "code": code,
            "grant_type": "authorization_code",
            "redirect_uri": redirect_uri
        }),
        OAuthProvider::GitHub => json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "code": code,
            "redirect_uri": redirect_uri
        }),
    };

    let resp = req.json(&body).send().await.map_err(|e| AppError::Http(e))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Request(format!("OAuth token 交换失败: HTTP {} {}", status, text)));
    }

    let token_resp: SocialTokenResponse = resp.json().await.map_err(|e| AppError::Http(e))?;
    Ok(token_resp)
}

/// Social OAuth token response.
#[derive(Debug, Deserialize)]
pub struct SocialTokenResponse {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    pub token_type: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Exchange social token for Kiro token via the social auth endpoint.
pub async fn exchange_for_kiro_token(
    client: &Client,
    social_token: &str,
    provider: OAuthProvider,
    region: &str,
) -> Result<(String, String, Option<i64>)> {
    let url = format!(
        "https://prod.{}.auth.desktop.kiro.dev/social/exchange",
        region
    );

    let provider_str = match provider {
        OAuthProvider::Google => "google",
        OAuthProvider::GitHub => "github",
    };

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&json!({
            "token": social_token,
            "provider": provider_str
        }))
        .send()
        .await
        .map_err(|e| AppError::Http(e))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Request(format!("Kiro social exchange 失败: HTTP {} {}", status, body)));
    }

    let data: Value = resp.json().await.map_err(|e| AppError::Http(e))?;

    let access_token = data.get("accessToken").and_then(|v| v.as_str()).ok_or_else(|| {
        AppError::Request("Social exchange 响应缺少 accessToken".to_string())
    })?.to_string();

    let refresh_token = data.get("refreshToken").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let expires_in = data.get("expiresIn").and_then(|v| v.as_i64());

    Ok((access_token, refresh_token, expires_in))
}

/// Generate a simple UUID-like string.
fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{:016x}{:016x}", t >> 64, t & 0xFFFFFFFFFFFFFFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_provider_urls() {
        assert!(OAuthProvider::Google.auth_url().contains("google"));
        assert!(OAuthProvider::GitHub.auth_url().contains("github"));
    }

    #[test]
    fn social_auth_url_format() {
        // This test requires env vars, so just check the function doesn't panic
        // when env vars are missing
        let result = start_social_auth(OAuthProvider::Google, "http://localhost:19823/callback", "test_state");
        // Should fail because env var is not set
        assert!(result.is_err());
    }
}
