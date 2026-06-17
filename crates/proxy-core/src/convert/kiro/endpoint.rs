//! Kiro upstream endpoint definitions and multi-endpoint dispatch support.
//!
//! Defines the three known Kiro API endpoints (Kiro IDE, CodeWhisperer, AmazonQ)
//! with per-endpoint URL, headers, and fallback logic on 429.

/// A Kiro upstream endpoint variant.
#[derive(Debug, Clone)]
pub struct KiroEndpoint {
    /// Human-readable name for logging.
    pub name: &'static str,
    /// URL template with `{region}` placeholder.
    pub url_template: &'static str,
    /// Origin header value.
    pub origin: &'static str,
    /// x-amz-target header value (None for Kiro IDE which doesn't set this).
    pub amz_target: Option<&'static str>,
}

/// The three known Kiro endpoints, in default priority order.
pub static KIRO_ENDPOINTS: [KiroEndpoint; 3] = [
    KiroEndpoint {
        name: "kiro",
        url_template: "https://q.{region}.amazonaws.com/generateAssistantResponse",
        origin: "AI_EDITOR",
        amz_target: None,
    },
    KiroEndpoint {
        name: "codewhisperer",
        url_template: "https://codewhisperer.{region}.amazonaws.com/generateAssistantResponse",
        origin: "AI_EDITOR",
        amz_target: Some("AmazonCodeWhispererStreamingService.GenerateAssistantResponse"),
    },
    KiroEndpoint {
        name: "amazonq",
        url_template: "https://q.{region}.amazonaws.com/generateAssistantResponse",
        origin: "AI_EDITOR",
        amz_target: Some("AmazonQDeveloperStreamingService.SendMessage"),
    },
];

/// Preferred endpoint selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PreferredEndpoint {
    /// Try Kiro IDE first, fall back to others on 429.
    #[default]
    Auto,
    /// Kiro IDE only (no fallback).
    Kiro,
    /// CodeWhisperer only.
    Codewhisperer,
    /// AmazonQ only.
    AmazonQ,
}


impl PreferredEndpoint {
    /// Convert from config string.
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s.unwrap_or("auto") {
            "kiro" => Self::Kiro,
            "codewhisperer" | "cw" => Self::Codewhisperer,
            "amazonq" | "amazon_q" | "q" => Self::AmazonQ,
            _ => Self::Auto,
        }
    }
}

/// Build sorted endpoint list from preference + fallback toggle.
/// Returns endpoints in priority order for dispatch.
pub fn get_sorted_endpoints(
    preferred: PreferredEndpoint,
    fallback_enabled: bool,
) -> Vec<&'static KiroEndpoint> {
    match preferred {
        PreferredEndpoint::Auto => {
            if fallback_enabled {
                KIRO_ENDPOINTS.iter().collect()
            } else {
                vec![&KIRO_ENDPOINTS[0]]
            }
        }
        PreferredEndpoint::Kiro => vec![&KIRO_ENDPOINTS[0]],
        PreferredEndpoint::Codewhisperer => {
            if fallback_enabled {
                vec![&KIRO_ENDPOINTS[1], &KIRO_ENDPOINTS[0], &KIRO_ENDPOINTS[2]]
            } else {
                vec![&KIRO_ENDPOINTS[1]]
            }
        }
        PreferredEndpoint::AmazonQ => {
            if fallback_enabled {
                vec![&KIRO_ENDPOINTS[2], &KIRO_ENDPOINTS[0], &KIRO_ENDPOINTS[1]]
            } else {
                vec![&KIRO_ENDPOINTS[2]]
            }
        }
    }
}

/// Build the full URL for a given endpoint by substituting the region.
pub fn build_endpoint_url(endpoint: &KiroEndpoint, region: &str) -> String {
    endpoint.url_template.replace("{region}", region)
}

/// Build HTTP headers for a specific Kiro endpoint.
///
/// Key difference from the old `build_kiro_headers`: the `x-amz-target` header
/// is only set when `endpoint.amz_target` is `Some(...)`.
pub fn build_endpoint_headers(
    endpoint: &KiroEndpoint,
    token: &str,
    amz_user_agent: &str,
    user_agent: &str,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".into(), "application/x-amz-json-1.0".into()),
        ("Authorization".into(), format!("Bearer {}", token)),
        ("x-amzn-codewhisperer-optout".into(), "true".into()),
        ("x-amzn-kiro-agent-mode".into(), "vibe".into()),
        ("x-amz-user-agent".into(), amz_user_agent.to_string()),
        ("user-agent".into(), user_agent.to_string()),
        ("amz-sdk-invocation-id".into(), uuid::Uuid::new_v4().to_string()),
        ("amz-sdk-request".into(), "attempt=1; max=3".into()),
    ];

    if let Some(target) = endpoint.amz_target {
        headers.push(("x-amz-target".into(), target.to_string()));
    }

    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_fallback_returns_all_three() {
        let eps = get_sorted_endpoints(PreferredEndpoint::Auto, true);
        assert_eq!(eps.len(), 3);
        assert_eq!(eps[0].name, "kiro");
        assert_eq!(eps[1].name, "codewhisperer");
        assert_eq!(eps[2].name, "amazonq");
    }

    #[test]
    fn auto_no_fallback_returns_one() {
        let eps = get_sorted_endpoints(PreferredEndpoint::Auto, false);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].name, "kiro");
    }

    #[test]
    fn codewhisperer_preferred_with_fallback() {
        let eps = get_sorted_endpoints(PreferredEndpoint::Codewhisperer, true);
        assert_eq!(eps.len(), 3);
        assert_eq!(eps[0].name, "codewhisperer");
    }

    #[test]
    fn endpoint_url_substitutes_region() {
        let url = build_endpoint_url(&KIRO_ENDPOINTS[0], "us-east-1");
        assert_eq!(url, "https://q.us-east-1.amazonaws.com/generateAssistantResponse");
    }

    #[test]
    fn kiro_endpoint_no_amz_target() {
        let headers = build_endpoint_headers(&KIRO_ENDPOINTS[0], "tok", "ua", "agent");
        assert!(!headers.iter().any(|(k, _)| k == "x-amz-target"));
    }

    #[test]
    fn codewhisperer_endpoint_has_amz_target() {
        let headers = build_endpoint_headers(&KIRO_ENDPOINTS[1], "tok", "ua", "agent");
        let target = headers.iter().find(|(k, _)| k == "x-amz-target").unwrap();
        assert_eq!(target.1, "AmazonCodeWhispererStreamingService.GenerateAssistantResponse");
    }
}
