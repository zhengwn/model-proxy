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

// ---- IAM IdC PKCE Flow ----

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// IAM IdC PKCE session state.
#[derive(Debug, Clone)]
struct IamSsoSession {
    client_id: String,
    client_secret: String,
    code_verifier: String,
    state: String,
    region: String,
    _start_url: String,
    created_at: Instant,
}

/// In-memory session store with 10-minute expiry.
static IAM_SSO_SESSIONS: Mutex<Option<HashMap<String, IamSsoSession>>> = Mutex::new(None);

const IAM_SSO_SESSION_TTL: Duration = Duration::from_secs(600);
const IAM_SSO_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

/// Start IAM IdC PKCE login flow.
/// Returns (session_id, authorize_url, expires_in).
pub async fn start_iam_sso_login(
    client: &reqwest::Client,
    start_url: &str,
    region: &str,
) -> std::result::Result<(String, String, u64), String> {
    let oidc_base = format!("https://oidc.{}.amazonaws.com", region);

    // Step 1: Register OIDC client
    let register_body = serde_json::json!({
        "clientName": "Kiro",
        "clientType": "public",
        "grantTypes": ["authorization_code", "refresh_token"],
        "redirectUris": ["http://127.0.0.1/oauth/callback"],
        "scopes": IAM_SSO_SCOPES,
        "issuerUrl": start_url,
    });

    let register_resp = client
        .post(format!("{}/client/register", oidc_base))
        .header("Content-Type", "application/json")
        .json(&register_body)
        .send()
        .await
        .map_err(|e| format!("OIDC client registration failed: {}", e))?;

    if !register_resp.status().is_success() {
        let status = register_resp.status().as_u16();
        let body = register_resp.text().await.unwrap_or_default();
        return Err(format!("OIDC registration failed ({}): {}", status, body));
    }

    let reg_data: serde_json::Value = register_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse OIDC registration response: {}", e))?;

    let client_id = reg_data["client_id"]
        .as_str()
        .ok_or("Missing client_id in registration response")?
        .to_string();
    let client_secret = reg_data["client_secret"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // Step 2: Generate PKCE
    let code_verifier = generate_pkce_verifier();
    let code_challenge = generate_pkce_challenge(&code_verifier);
    let state = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    // Step 3: Build authorize URL
    let scopes = IAM_SSO_SCOPES.join("%20");
    let authorize_url = format!(
        "{}/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        oidc_base,
        client_id,
        "http://127.0.0.1/oauth/callback",
        scopes,
        state,
        code_challenge,
    );

    // Step 4: Store session
    let session = IamSsoSession {
        client_id,
        client_secret,
        code_verifier,
        state,
        region: region.to_string(),
        _start_url: start_url.to_string(),
        created_at: Instant::now(),
    };

    let mut sessions = IAM_SSO_SESSIONS.lock().unwrap();
    if sessions.is_none() {
        *sessions = Some(HashMap::new());
    }
    // Clean up expired sessions
    if let Some(ref mut map) = *sessions {
        map.retain(|_, s| s.created_at.elapsed() < IAM_SSO_SESSION_TTL);
    }
    sessions.as_mut().unwrap().insert(session_id.clone(), session);

    Ok((session_id, authorize_url, IAM_SSO_SESSION_TTL.as_secs()))
}

/// Complete IAM IdC PKCE login.
/// Returns (access_token, refresh_token, client_id, client_secret, region, expires_in).
pub async fn complete_iam_sso_login(
    client: &reqwest::Client,
    session_id: &str,
    callback_url: &str,
) -> std::result::Result<(String, String, String, String, String, i64), String> {
    // Look up session
    let session = {
        let mut sessions = IAM_SSO_SESSIONS.lock().unwrap();
        let sessions_map = sessions.as_mut().ok_or("No active IAM SSO sessions")?;

        // Clean expired sessions before lookup
        sessions_map.retain(|_, s| s.created_at.elapsed() < IAM_SSO_SESSION_TTL);

        let session = sessions_map
            .remove(session_id)
            .ok_or("Session not found or expired")?;
        session
    };

    // Parse callback URL for code and state
    let query_str = callback_url.split('?').nth(1).unwrap_or("");
    let params: HashMap<String, String> = query_str
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?.to_string();
            let val = percent_decode(parts.next().unwrap_or(""));
            Some((key, val))
        })
        .collect();

    let code = params.get("code").ok_or("Missing 'code' in callback URL")?.clone();
    let returned_state = params.get("state").ok_or("Missing 'state' in callback URL")?.clone();

    if returned_state != session.state {
        return Err("State mismatch in callback URL".to_string());
    }

    // Exchange code for tokens
    let oidc_base = format!("https://oidc.{}.amazonaws.com", session.region);
    let token_body = serde_json::json!({
        "grantType": "authorization_code",
        "clientId": session.client_id,
        "clientSecret": session.client_secret,
        "code": code,
        "codeVerifier": session.code_verifier,
        "redirectUri": "http://127.0.0.1/oauth/callback",
    });

    let token_resp = client
        .post(format!("{}/token", oidc_base))
        .header("Content-Type", "application/json")
        .json(&token_body)
        .send()
        .await
        .map_err(|e| format!("Token exchange failed: {}", e))?;

    if !token_resp.status().is_success() {
        let status = token_resp.status().as_u16();
        let body = token_resp.text().await.unwrap_or_default();
        return Err(format!("Token exchange failed ({}): {}", status, body));
    }

    let token_data: serde_json::Value = token_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse token response: {}", e))?;

    let access_token = token_data["accessToken"]
        .as_str()
        .or_else(|| token_data["access_token"].as_str())
        .ok_or("Missing access_token")?
        .to_string();
    let refresh_token = token_data["refreshToken"]
        .as_str()
        .or_else(|| token_data["refresh_token"].as_str())
        .ok_or("Missing refresh_token")?
        .to_string();
    let expires_in = token_data["expiresIn"]
        .as_i64()
        .or_else(|| token_data["expires_in"].as_i64())
        .unwrap_or(3600);

    Ok((
        access_token,
        refresh_token,
        session.client_id,
        session.client_secret,
        session.region,
        expires_in,
    ))
}

// ---- SSO Token Import ----

/// Import credentials from an SSO bearer token via a 7-step automated flow.
/// Returns (access_token, refresh_token, client_id, client_secret, region, expires_in).
pub async fn import_from_sso_token(
    client: &reqwest::Client,
    bearer_token: &str,
    region: &str,
) -> std::result::Result<(String, String, String, String, String, i64), String> {
    let oidc_base = format!("https://oidc.{}.amazonaws.com", region);
    let portal_base = "https://portal.sso.us-east-1.amazonaws.com";

    // Step 1: Register device client
    let register_body = serde_json::json!({
        "clientName": "KiroProxy",
        "clientType": "public",
        "grantTypes": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
    });

    let reg_resp = client
        .post(format!("{}/client/register", oidc_base))
        .header("Content-Type", "application/json")
        .json(&register_body)
        .send()
        .await
        .map_err(|e| format!("Device client registration failed: {}", e))?;

    if !reg_resp.status().is_success() {
        let status = reg_resp.status().as_u16();
        let body = reg_resp.text().await.unwrap_or_default();
        return Err(format!("Device client registration failed ({}): {}", status, body));
    }

    let reg_data: serde_json::Value = reg_resp.json().await
        .map_err(|e| format!("Parse registration response: {}", e))?;
    let device_client_id = reg_data["client_id"].as_str()
        .ok_or("Missing client_id")?.to_string();

    // Step 2: Start device authorization
    let dev_auth_body = serde_json::json!({
        "clientId": device_client_id,
        "scope": IAM_SSO_SCOPES,
    });

    let dev_resp = client
        .post(format!("{}/device_authorization", oidc_base))
        .header("Content-Type", "application/json")
        .json(&dev_auth_body)
        .send()
        .await
        .map_err(|e| format!("Device authorization failed: {}", e))?;

    if !dev_resp.status().is_success() {
        let status = dev_resp.status().as_u16();
        let body = dev_resp.text().await.unwrap_or_default();
        return Err(format!("Device authorization failed ({}): {}", status, body));
    }

    let dev_data: serde_json::Value = dev_resp.json().await
        .map_err(|e| format!("Parse device auth response: {}", e))?;
    let device_code = dev_data["device_code"].as_str()
        .ok_or("Missing device_code")?.to_string();
    let user_code = dev_data["user_code"].as_str()
        .ok_or("Missing user_code")?.to_string();

    // Step 3: Verify bearer token
    let whoami_resp = client
        .get(format!("{}/token/whoAmI", portal_base))
        .header("x-amz-sso_bearer_token", bearer_token)
        .send()
        .await
        .map_err(|e| format!("Bearer token verification failed: {}", e))?;

    if !whoami_resp.status().is_success() {
        let status = whoami_resp.status().as_u16();
        return Err(format!("Invalid bearer token ({}): token verification failed", status));
    }

    // Step 4: Get device session token
    let session_resp = client
        .post(format!("{}/session/device", portal_base))
        .header("x-amz-sso_bearer_token", bearer_token)
        .send()
        .await
        .map_err(|e| format!("Device session request failed: {}", e))?;

    if !session_resp.status().is_success() {
        let status = session_resp.status().as_u16();
        return Err(format!("Device session failed ({}): {}", status, session_resp.text().await.unwrap_or_default()));
    }

    let session_data: serde_json::Value = session_resp.json().await
        .map_err(|e| format!("Parse session response: {}", e))?;
    let device_session_id = session_data["deviceSessionId"]
        .as_str()
        .or_else(|| session_data["sessionId"].as_str())
        .ok_or("Missing deviceSessionId")?
        .to_string();

    // Step 5: Accept user code
    let accept_body = serde_json::json!({
        "userCode": user_code,
        "userSessionId": device_session_id,
    });

    let accept_resp = client
        .post(format!("{}/device_authorization/accept_user_code", oidc_base))
        .header("Content-Type", "application/json")
        .json(&accept_body)
        .send()
        .await
        .map_err(|e| format!("Accept user code failed: {}", e))?;

    if !accept_resp.status().is_success() {
        let status = accept_resp.status().as_u16();
        let body = accept_resp.text().await.unwrap_or_default();
        return Err(format!("Accept user code failed ({}): {}", status, body));
    }

    // Step 6: Approve authorization
    let assoc_body = serde_json::json!({
        "userCode": user_code,
        "deviceSessionId": device_session_id,
        "clientId": device_client_id,
    });

    let assoc_resp = client
        .post(format!("{}/device_authorization/associate_token", oidc_base))
        .header("Content-Type", "application/json")
        .json(&assoc_body)
        .send()
        .await
        .map_err(|e| format!("Associate token failed: {}", e))?;

    if !assoc_resp.status().is_success() {
        let status = assoc_resp.status().as_u16();
        let body = assoc_resp.text().await.unwrap_or_default();
        return Err(format!("Associate token failed ({}): {}", status, body));
    }

    // Step 7: Poll for token
    let poll_body = serde_json::json!({
        "grantType": "urn:ietf:params:oauth:grant-type:device_code",
        "deviceCode": device_code,
        "clientId": device_client_id,
    });

    // Poll with timeout
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_error = String::new();

    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(3)).await;

        let poll_resp = client
            .post(format!("{}/token", oidc_base))
            .header("Content-Type", "application/json")
            .json(&poll_body)
            .send()
            .await;

        match poll_resp {
            Ok(resp) if resp.status().is_success() => {
                let token_data: serde_json::Value = resp.json().await
                    .map_err(|e| format!("Parse token response: {}", e))?;

                let access_token = token_data["accessToken"]
                    .as_str()
                    .or_else(|| token_data["access_token"].as_str())
                    .ok_or("Missing access_token")?
                    .to_string();
                let refresh_token = token_data["refreshToken"]
                    .as_str()
                    .or_else(|| token_data["refresh_token"].as_str())
                    .ok_or("Missing refresh_token")?
                    .to_string();
                let expires_in = token_data["expiresIn"]
                    .as_i64()
                    .or_else(|| token_data["expires_in"].as_i64())
                    .unwrap_or(3600);

                return Ok((
                    access_token,
                    refresh_token,
                    device_client_id,
                    String::new(),
                    region.to_string(),
                    expires_in,
                ));
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                last_error = format!("{}: {}", status, body);
                // Continue polling for pending states
                if status == 400 && (body.contains("authorization_pending") || body.contains("slow_down")) {
                    continue;
                }
                // Non-retryable error
                return Err(format!("Token poll failed: {}", last_error));
            }
            Err(e) => {
                last_error = format!("{}", e);
            }
        }
    }

    Err(format!("SSO token import timed out: {}", last_error))
}

// ---- PKCE helpers ----

/// Generate a PKCE code verifier (32 random bytes, base64url encoded).
fn generate_pkce_verifier() -> String {
    let mut bytes = [0u8; 32];
    getrandom(&mut bytes);
    base64url_encode(&bytes)
}

/// Generate PKCE code challenge (SHA256 of verifier, base64url encoded).
fn generate_pkce_challenge(verifier: &str) -> String {
    let hash = sha2::Sha256::digest(verifier.as_bytes());
    base64url_encode(&hash)
}

/// Simple percent-decoding.
fn percent_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            result.push(b' ');
        } else {
            result.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8(result).unwrap_or_default()
}

/// Simple base64url encoding (no padding).
fn base64url_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).map(|&b| b as u32).unwrap_or(0);
        let b2 = chunk.get(2).map(|&b| b as u32).unwrap_or(0);
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        }
    }
    result
}

/// Fill buffer with random bytes using getrandom.
fn getrandom(buf: &mut [u8]) {
    // Use std random source (platform-specific)
    #[cfg(target_os = "macos")]
    {
        // On macOS, use Security.framework via /dev/urandom fallback
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom").expect("Failed to open /dev/urandom");
        f.read_exact(buf).expect("Failed to read random bytes");
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom").expect("Failed to open /dev/urandom");
        f.read_exact(buf).expect("Failed to read random bytes");
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // Use uuid v4 (CSPRNG-backed) for cross-platform random bytes
        let uuid_bytes = uuid::Uuid::new_v4();
        buf.copy_from_slice(uuid_bytes.as_bytes());
    }
}

// Need sha2 for PKCE challenge
use sha2::Digest;

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
