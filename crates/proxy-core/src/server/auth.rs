//! Client authentication middleware and helpers.
//!
//! Supports:
//! - Global API key authentication (x-api-key or Authorization: Bearer)
//! - Multi-tenant format: `API_KEY:REFRESH_TOKEN`
//! - Constant-time comparison to prevent timing attacks

use axum::{
    body::Body,
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::config::Config;
use crate::error::{AppError, Result};

/// Multi-tenant auth result.
#[derive(Debug)]
pub enum AuthResult {
    /// Global API key matched, or no API key configured
    GlobalOk,
    /// Multi-tenant: extracted per-request refresh token
    TenantOk { refresh_token: String },
    /// Authentication failed
    Unauthorized,
}

/// Check authentication, supporting both global API key and multi-tenant
/// `API_KEY:REFRESH_TOKEN` format.
///
/// The check order is:
/// 1. If no `server.api_key` configured → always allow (GlobalOk)
/// 2. Try exact full-key match first (handles keys containing colons)
/// 3. Try multi-tenant split on first colon
/// 4. Otherwise → Unauthorized
pub fn check_auth_tenant(headers: &HeaderMap, config: &Config) -> AuthResult {
    let Some(expected_key) = &config.server.api_key else {
        return AuthResult::GlobalOk;
    };

    let provided = headers
        .get("x-api-key")
        .or_else(|| headers.get("authorization"))
        .and_then(|v| v.to_str().ok());

    let provided_clean = provided.map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim());

    match provided_clean {
        Some(key) => {
            // First, try standard single-key check (handles keys that contain colons)
            let key_bytes = key.as_bytes();
            let expected_bytes = expected_key.as_bytes();
            if key_bytes.len() == expected_bytes.len() && key_bytes.ct_eq(expected_bytes).into() {
                return AuthResult::GlobalOk;
            }

            // Check for multi-tenant format: API_KEY:REFRESH_TOKEN
            if let Some((api_key_part, refresh_token)) = key.split_once(':') {
                if !refresh_token.is_empty() {
                    let part_bytes = api_key_part.as_bytes();
                    if part_bytes.len() == expected_bytes.len()
                        && part_bytes.ct_eq(expected_bytes).into()
                    {
                        return AuthResult::TenantOk {
                            refresh_token: refresh_token.to_string(),
                        };
                    }
                }
            }

            warn!("API key 验证失败");
            AuthResult::Unauthorized
        }
        None => {
            warn!("API key 验证失败");
            AuthResult::Unauthorized
        }
    }
}

/// Simple auth check that returns Ok/Err (no multi-tenant info).
pub(crate) fn check_auth(headers: &HeaderMap, config: &Config) -> Result<()> {
    match check_auth_tenant(headers, config) {
        AuthResult::GlobalOk | AuthResult::TenantOk { .. } => Ok(()),
        AuthResult::Unauthorized => Err(AppError::Unauthorized),
    }
}

/// Axum middleware that enforces client API key authentication.
/// Skip auth for public endpoints: /health, /metrics, /v1/models.
pub async fn client_auth_middleware(
    State(state): State<super::state::AppState>,
    headers: HeaderMap,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let is_public = matches!(path, "/health" | "/metrics" | "/v1/models");
    if !is_public {
        if let Err(e) = check_auth(&headers, &state.config) {
            return e.into_response();
        }
    }
    next.run(req).await
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use crate::config::{Config, ServerConfig};

    fn make_config(api_key: Option<&str>) -> Config {
        Config {
            server: ServerConfig {
                port: 4000,
                host: "127.0.0.1".to_string(),
                api_key: api_key.map(|s| s.to_string()),
                admin_api_key: None,
                max_body_bytes: 64 * 1024 * 1024,
                max_concurrent_requests: 0,
            },
            provider: crate::config::ProviderConfig::placeholder(),
            active_provider: None,
            providers: vec![],
            model_routes: vec![],
            model_routes_enabled: true,
            logging: Default::default(),
            fallback: Default::default(),
        }
    }

    fn headers_with(key: &str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HeaderName::from_bytes(key.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        headers
    }

    // --- No API key configured ---

    #[test]
    fn no_api_key_configured_allows_all() {
        let config = make_config(None);
        let headers = HeaderMap::new();
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::GlobalOk));
    }

    #[test]
    fn no_api_key_configured_ignores_provided_key() {
        let config = make_config(None);
        let headers = headers_with("x-api-key", "anything");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::GlobalOk));
    }

    // --- Valid API key via x-api-key header ---

    #[test]
    fn valid_x_api_key_header() {
        let config = make_config(Some("secret123"));
        let headers = headers_with("x-api-key", "secret123");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::GlobalOk));
    }

    #[test]
    fn invalid_x_api_key_header() {
        let config = make_config(Some("secret123"));
        let headers = headers_with("x-api-key", "wrong");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::Unauthorized));
    }

    // --- Valid API key via Authorization: Bearer ---

    #[test]
    fn valid_bearer_token() {
        let config = make_config(Some("my-key"));
        let headers = headers_with("authorization", "Bearer my-key");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::GlobalOk));
    }

    #[test]
    fn bearer_with_extra_spaces_trimmed() {
        let config = make_config(Some("my-key"));
        let headers = headers_with("authorization", "Bearer  my-key ");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::GlobalOk));
    }

    #[test]
    fn invalid_bearer_token() {
        let config = make_config(Some("my-key"));
        let headers = headers_with("authorization", "Bearer wrong-key");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::Unauthorized));
    }

    // --- No credentials provided ---

    #[test]
    fn missing_credentials_when_key_configured() {
        let config = make_config(Some("secret"));
        let headers = HeaderMap::new();
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::Unauthorized));
    }

    // --- Multi-tenant format ---

    #[test]
    fn multi_tenant_format_valid() {
        let config = make_config(Some("api-key"));
        let headers = headers_with("x-api-key", "api-key:refresh-token-abc");
        match check_auth_tenant(&headers, &config) {
            AuthResult::TenantOk { refresh_token } => {
                assert_eq!(refresh_token, "refresh-token-abc");
            }
            other => panic!("Expected TenantOk, got {:?}", other),
        }
    }

    #[test]
    fn multi_tenant_format_wrong_api_key_part() {
        let config = make_config(Some("api-key"));
        let headers = headers_with("x-api-key", "wrong-key:refresh-token");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::Unauthorized));
    }

    #[test]
    fn multi_tenant_format_empty_refresh_token() {
        // "api-key:" — colon present but empty refresh token → falls through to full match
        let config = make_config(Some("api-key:"));
        let headers = headers_with("x-api-key", "api-key:");
        // The full key "api-key:" matches the configured key "api-key:" exactly
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::GlobalOk));
    }

    // --- API key containing colons ---

    #[test]
    fn api_key_with_colon_exact_match_takes_priority() {
        // If the configured key itself contains a colon, exact match should win
        let config = make_config(Some("sk-ant:api03:xxxx"));
        let headers = headers_with("x-api-key", "sk-ant:api03:xxxx");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::GlobalOk));
    }

    #[test]
    fn api_key_with_colon_no_false_positive_tenant() {
        // A key like "sk-ant:something" should NOT be interpreted as
        // api_key="sk-ant" + refresh_token="something" when configured key is "sk-ant:something"
        let config = make_config(Some("sk-ant:something"));
        let headers = headers_with("x-api-key", "sk-ant:something");
        // Should match as GlobalOk (exact match), not TenantOk
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::GlobalOk));
    }

    // --- Constant-time comparison (length mismatch rejection) ---

    #[test]
    fn different_length_key_rejected() {
        let config = make_config(Some("short"));
        let headers = headers_with("x-api-key", "this-is-a-much-longer-key");
        assert!(matches!(check_auth_tenant(&headers, &config), AuthResult::Unauthorized));
    }

    // --- check_auth helper ---

    #[test]
    fn check_auth_ok_when_no_key_configured() {
        let config = make_config(None);
        assert!(check_auth(&HeaderMap::new(), &config).is_ok());
    }

    #[test]
    fn check_auth_err_when_unauthorized() {
        let config = make_config(Some("secret"));
        assert!(check_auth(&HeaderMap::new(), &config).is_err());
    }

    #[test]
    fn check_auth_ok_for_tenant() {
        let config = make_config(Some("key"));
        let headers = headers_with("x-api-key", "key:token");
        assert!(check_auth(&headers, &config).is_ok());
    }
}
