//! Kiro credential types: auth method, source, and the KiroCredential struct.

use chrono::{DateTime, Duration, Utc};

use super::fingerprint::generate_machine_id;
use crate::config::KiroConfig;

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
