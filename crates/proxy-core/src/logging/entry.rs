use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    pub timestamp: String, // ISO 8601 UTC
    pub method: String,    // HTTP method
    pub path: String,      // Request path
    pub provider: String,  // Provider name
    pub model: String,     // Resolved/actual model name sent to provider
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>, // Original model name from client request
    pub status: u16,       // Response status code
    pub duration_ms: u64,  // Total request duration (end-to-end)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_overhead_ms: Option<u64>, // Time spent in proxy before sending upstream
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>, // Time to first token (upstream headers received)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    pub is_stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u64>,
}
