//! Prompt caching support for Kiro API.
//!
//! Two layers:
//! 1. Format conversion: Anthropic `cache_control` → Kiro `cachePoint`
//! 2. Cache tracking: SHA256 fingerprint-based cache simulation for computing
//!    `cache_creation_input_tokens` and `cache_read_input_tokens` in responses.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::debug;

// ---- Format conversion (existing) ----

/// Convert Anthropic cache_control markers to Kiro cachePoint format.
pub fn convert_cache_control(body: &mut Value) {
    let mut has_cache_markers = false;

    if let Some(system) = body.get_mut("system") {
        if let Value::Array(blocks) = system {
            for block in blocks.iter_mut() {
                if block.get("cache_control").is_some() {
                    has_cache_markers = true;
                    if let Some(obj) = block.as_object_mut() {
                        obj.remove("cache_control");
                    }
                }
            }
        }
    }

    if let Some(messages) = body.get_mut("messages") {
        if let Value::Array(msgs) = messages {
            for msg in msgs.iter_mut() {
                if let Some(content) = msg.get_mut("content") {
                    if let Value::Array(blocks) = content {
                        for block in blocks.iter_mut() {
                            if block.get("cache_control").is_some() {
                                has_cache_markers = true;
                                if let Some(obj) = block.as_object_mut() {
                                    obj.remove("cache_control");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(tools) = body.get_mut("tools") {
        if let Value::Array(tool_arr) = tools {
            for tool in tool_arr.iter_mut() {
                if tool.get("cache_control").is_some() {
                    has_cache_markers = true;
                    if let Some(obj) = tool.as_object_mut() {
                        obj.remove("cache_control");
                        obj.insert("cachePoint".to_string(), json!(true));
                    }
                }
            }
        }
    }

    if has_cache_markers {
        debug!("Prompt caching markers detected and converted");
    }
}

/// Add cachePoint markers to the Kiro conversationState history.
pub fn add_history_cache_points(conversation_state: &mut Value, system_history_len: usize) {
    if let Some(history) = conversation_state.get_mut("history").and_then(|v| v.as_array_mut()) {
        for (i, entry) in history.iter_mut().enumerate() {
            if i < system_history_len {
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("cachePoint".to_string(), json!(true));
                }
            }
        }
    }
}

// ---- SHA256 Cache Tracker ----

const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes
const ONE_HOUR_TTL: Duration = Duration::from_secs(3600);
const DEFAULT_MIN_CACHEABLE_TOKENS: usize = 1024;
const OPUS_MIN_CACHEABLE_TOKENS: usize = 4096;
const CACHEABLE_CAP_RATIO: f64 = 0.85;

/// Cache usage stats to include in response.
#[derive(Debug, Clone, Default)]
pub struct PromptCacheUsage {
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_5m_input_tokens: u64,
    pub cache_creation_1h_input_tokens: u64,
}

impl PromptCacheUsage {
    pub fn is_empty(&self) -> bool {
        self.cache_creation_input_tokens == 0 && self.cache_read_input_tokens == 0
    }
}

/// A fingerprint captured at a cache breakpoint.
#[derive(Debug, Clone)]
struct CacheBreakpoint {
    fingerprint: [u8; 32],
    cumulative_tokens: usize,
    ttl: Duration,
}

/// Profile built from a single request — contains all breakpoints.
#[derive(Debug, Clone)]
pub struct PromptCacheProfile {
    breakpoints: Vec<CacheBreakpoint>,
    total_input_tokens: usize,
    _model: String,
}

/// Per-account cache entry with TTL.
#[derive(Debug, Clone)]
struct CacheEntry {
    expires_at: Instant,
    ttl: Duration,
}

/// Thread-safe prompt cache tracker.
///
/// Tracks SHA256 fingerprints at cache_control breakpoints per account.
/// On first request, reports all tokens as cache_creation.
/// On subsequent requests with matching prefixes, reports matched portion as cache_read.
pub struct PromptCacheTracker {
    entries: Mutex<HashMap<String, HashMap<[u8; 32], CacheEntry>>>,
}

impl PromptCacheTracker {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Build a cache profile from an Anthropic request body.
    /// Returns None if no cache_control markers found.
    pub fn build_profile(
        &self,
        body: &Value,
        total_input_tokens: usize,
        model: &str,
    ) -> Option<PromptCacheProfile> {
        let blocks = flatten_cache_blocks(body);
        if blocks.is_empty() {
            return None;
        }

        let mut hasher = Sha256::new();
        let mut breakpoints = Vec::new();
        let mut cumulative_tokens = 0usize;
        let mut seen_explicit = false;

        for block in &blocks {
            let canonical = canonicalize_cache_value(&block.value);
            write_hash_chunk(&mut hasher, &canonical);
            cumulative_tokens += block.tokens;

            if block.has_cache_control {
                seen_explicit = true;
            }

            // Capture fingerprint at cache_control breakpoints (explicit or implicit message-end)
            if block.has_cache_control || (seen_explicit && block.is_message_end) {
                let fingerprint: [u8; 32] = hasher.clone().finalize().into();
                breakpoints.push(CacheBreakpoint {
                    fingerprint,
                    cumulative_tokens,
                    ttl: block.ttl,
                });
            }
        }

        if breakpoints.is_empty() {
            return None;
        }

        Some(PromptCacheProfile {
            breakpoints,
            total_input_tokens,
            _model: model.to_string(),
        })
    }

    /// Compute cache usage for a given account and profile.
    pub fn compute(&self, account_id: &str, profile: &PromptCacheProfile) -> PromptCacheUsage {
        let min_tokens = min_cacheable_tokens(profile._model.as_str());

        let mut entries = self.entries.lock().unwrap();
        let account_entries = entries.entry(account_id.to_string()).or_default();

        // Remove expired entries
        let now = Instant::now();
        account_entries.retain(|_, entry| entry.expires_at > now);

        // Find the deepest matching breakpoint (largest prefix match)
        let mut matched_tokens = 0usize;
        for bp in profile.breakpoints.iter().rev() {
            if account_entries.contains_key(&bp.fingerprint) && bp.cumulative_tokens >= min_tokens {
                matched_tokens = bp.cumulative_tokens;
                break;
            }
        }

        let total = profile.total_input_tokens.max(1);
        let capped_total = ((total as f64 * CACHEABLE_CAP_RATIO) as usize).max(min_tokens);

        if matched_tokens > 0 {
            let cache_read = matched_tokens.min(capped_total);
            let remaining = total.saturating_sub(cache_read);
            let cache_creation = remaining.min(capped_total.saturating_sub(cache_read));

            let (creation_5m, creation_1h) = compute_ttl_breakdown(profile, cache_creation);

            PromptCacheUsage {
                cache_creation_input_tokens: cache_creation as u64,
                cache_read_input_tokens: cache_read as u64,
                cache_creation_5m_input_tokens: creation_5m as u64,
                cache_creation_1h_input_tokens: creation_1h as u64,
            }
        } else {
            // First request for this account — all tokens are creation
            let cache_creation = capped_total.min(total);
            let (creation_5m, creation_1h) = compute_ttl_breakdown(profile, cache_creation);

            PromptCacheUsage {
                cache_creation_input_tokens: cache_creation as u64,
                cache_read_input_tokens: 0,
                cache_creation_5m_input_tokens: creation_5m as u64,
                cache_creation_1h_input_tokens: creation_1h as u64,
            }
        }
    }

    /// Update stored fingerprints after a successful request.
    pub fn update(&self, account_id: &str, profile: &PromptCacheProfile) {
        let mut entries = self.entries.lock().unwrap();
        let account_entries = entries.entry(account_id.to_string()).or_default();

        let now = Instant::now();
        for bp in &profile.breakpoints {
            let ttl = bp.ttl.min(ONE_HOUR_TTL);
            account_entries.insert(
                bp.fingerprint,
                CacheEntry {
                    expires_at: now + ttl,
                    ttl,
                },
            );
        }
    }
}

impl Default for PromptCacheTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Internal helpers ----

/// A single cacheable block extracted from the request.
struct CacheableBlock {
    value: Value,
    tokens: usize,
    ttl: Duration,
    has_cache_control: bool,
    is_message_end: bool,
}

/// Decompose an Anthropic request into cacheable blocks.
/// Order: prelude → tools → system → messages
fn flatten_cache_blocks(body: &Value) -> Vec<CacheableBlock> {
    let mut blocks = Vec::new();

    // Prelude: model + tool_choice
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let tool_choice = body.get("tool_choice");
    let prelude = json!({
        "kind": "request_prelude",
        "model": model,
        "tool_choice": tool_choice,
    });
    blocks.push(CacheableBlock {
        value: prelude,
        tokens: estimate_approx_tokens(model) + 4,
        ttl: DEFAULT_CACHE_TTL,
        has_cache_control: false,
        is_message_end: false,
    });

    // Tools
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        for tool in tools {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let schema = tool.get("input_schema");
            let cache_control = tool.get("cache_control");
            let has_cc = cache_control.is_some();
            let ttl = extract_cache_ttl(cache_control);

            let val = json!({
                "kind": "tool",
                "name": name,
                "description": desc,
                "input_schema": schema,
            });
            let tokens = estimate_approx_tokens(name) + estimate_approx_tokens(desc)
                + schema.map(|s| estimate_json_tokens(s)).unwrap_or(0);

            blocks.push(CacheableBlock {
                value: val,
                tokens,
                ttl,
                has_cache_control: has_cc,
                is_message_end: false,
            });
        }
    }

    // System prompt
    if let Some(system) = body.get("system") {
        let system_blocks = if let Value::Array(arr) = system {
            arr.clone()
        } else if let Some(s) = system.as_str() {
            vec![json!(s)]
        } else {
            vec![]
        };

        for block in &system_blocks {
            let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let cache_control = block.get("cache_control");
            let has_cc = cache_control.is_some();
            let ttl = extract_cache_ttl(cache_control);

            blocks.push(CacheableBlock {
                value: json!({"kind": "system", "text": text}),
                tokens: estimate_approx_tokens(text),
                ttl,
                has_cache_control: has_cc,
                is_message_end: false,
            });
        }
    }

    // Messages
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = match msg.get("content") {
                Some(Value::Array(arr)) => arr.clone(),
                Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
                _ => vec![],
            };

            let content_len = content.len();
            for (i, block) in content.iter().enumerate() {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                let text = match block_type {
                    "text" => block.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    "tool_result" => {
                        // Serialize content of tool_result
                        block.get("content").map(|c| c.to_string()).unwrap_or_default()
                    }
                    _ => block.to_string(),
                };
                let cache_control = block.get("cache_control");
                let has_cc = cache_control.is_some();
                let ttl = extract_cache_ttl(cache_control);

                blocks.push(CacheableBlock {
                    value: json!({
                        "kind": "message",
                        "role": role,
                        "block_type": block_type,
                        "text": text,
                    }),
                    tokens: estimate_approx_tokens(&text) + 4, // +4 for role overhead
                    ttl,
                    has_cache_control: has_cc,
                    is_message_end: i == content_len - 1,
                });
            }
        }
    }

    blocks
}

/// Produce deterministic JSON string for hashing.
fn canonicalize_cache_value(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map
                .iter()
                .filter(|(k, _)| *k != "cache_control" && *k != "cache_control_type")
                .collect();
            entries.sort_by_key(|(k, _)| k.as_str());
            let inner: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{}:{}", canonical_json_str(k), canonicalize_cache_value(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(canonicalize_cache_value).collect();
            format!("[{}]", items.join(","))
        }
        Value::String(s) => canonical_json_str(s),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
    }
}

fn canonical_json_str(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{}\"", escaped)
}

/// Write a length-prefixed, null-delimited chunk to the hasher.
fn write_hash_chunk(hasher: &mut Sha256, chunk: &str) {
    hasher.update(chunk.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(chunk.as_bytes());
    hasher.update(b"\0");
}

/// Extract TTL from cache_control metadata.
fn extract_cache_ttl(cache_control: Option<&Value>) -> Duration {
    if let Some(cc) = cache_control {
        if let Some(ttl_val) = cc.get("ttl") {
            if let Some(secs) = ttl_val.as_u64() {
                let d = Duration::from_secs(secs);
                return d.min(ONE_HOUR_TTL);
            }
            if let Some(s) = ttl_val.as_str() {
                // Parse simple duration strings like "1h", "5m", "300s"
                let trimmed = s.trim();
                if let Some(num_str) = trimmed.strip_suffix('h') {
                    if let Ok(h) = num_str.trim().parse::<u64>() {
                        return Duration::from_secs(h * 3600).min(ONE_HOUR_TTL);
                    }
                }
                if let Some(num_str) = trimmed.strip_suffix('m') {
                    if let Ok(m) = num_str.trim().parse::<u64>() {
                        return Duration::from_secs(m * 60).min(ONE_HOUR_TTL);
                    }
                }
                if let Some(num_str) = trimmed.strip_suffix('s') {
                    if let Ok(s) = num_str.trim().parse::<u64>() {
                        return Duration::from_secs(s).min(ONE_HOUR_TTL);
                    }
                }
                if let Ok(secs) = trimmed.parse::<u64>() {
                    return Duration::from_secs(secs).min(ONE_HOUR_TTL);
                }
            }
        }
    }
    DEFAULT_CACHE_TTL
}

/// Minimum cacheable tokens threshold based on model.
fn min_cacheable_tokens(model: &str) -> usize {
    if model.contains("opus") {
        OPUS_MIN_CACHEABLE_TOKENS
    } else {
        DEFAULT_MIN_CACHEABLE_TOKENS
    }
}

/// Compute TTL breakdown into 5m and 1h buckets.
fn compute_ttl_breakdown(profile: &PromptCacheProfile, cache_creation: usize) -> (usize, usize) {
    let mut tokens_5m = 0usize;
    let mut tokens_1h = 0usize;
    let mut prev_tokens = 0usize;

    for bp in &profile.breakpoints {
        let delta = bp.cumulative_tokens.saturating_sub(prev_tokens);
        if bp.ttl <= DEFAULT_CACHE_TTL {
            tokens_5m += delta;
        } else {
            tokens_1h += delta;
        }
        prev_tokens = bp.cumulative_tokens;
    }

    // Scale by the ratio of cache_creation to total breakpoint tokens
    let total_bp_tokens = profile
        .breakpoints
        .last()
        .map(|bp| bp.cumulative_tokens)
        .unwrap_or(1)
        .max(1);
    let ratio = cache_creation as f64 / total_bp_tokens as f64;

    (
        (tokens_5m as f64 * ratio) as usize,
        (tokens_1h as f64 * ratio) as usize,
    )
}

// ---- Token estimation (character-class based, matching kiro-go) ----

/// Estimate token count using character-class heuristics.
pub fn estimate_approx_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let rune_count = text.chars().count();
    if rune_count < 5 {
        return ((text.len() + 2) / 3).max(1);
    }

    let mut regular_ascii = 0f64;
    let mut digits = 0f64;
    let mut symbols = 0f64;
    let mut non_ascii = 0f64;

    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits += 1.0;
        } else if ch.is_ascii_alphabetic() || ch == ' ' {
            regular_ascii += 1.0;
        } else if ch.is_ascii() {
            symbols += 1.0;
        } else {
            non_ascii += 1.0;
        }
    }

    ((regular_ascii / 4.5 + digits / 2.0 + symbols / 1.5 + non_ascii / 1.5).ceil() as usize).max(1)
}

/// Estimate tokens for a JSON value by serializing it.
fn estimate_json_tokens(value: &Value) -> usize {
    estimate_approx_tokens(&value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Existing tests ----

    #[test]
    fn convert_cache_control_on_tools() {
        let mut body = json!({
            "tools": [
                {"name": "test", "cache_control": {"type": "ephemeral"}},
                {"name": "other"}
            ]
        });
        convert_cache_control(&mut body);
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][0]["cachePoint"], true);
        assert!(body["tools"][1].get("cachePoint").is_none());
    }

    #[test]
    fn convert_cache_control_on_messages() {
        let mut body = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}
                    ]
                }
            ]
        });
        convert_cache_control(&mut body);
        assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn add_history_cache_points_test() {
        let mut state = json!({
            "history": [
                {"userInputMessage": {"content": "system"}},
                {"assistantResponseMessage": {"content": "OK"}},
                {"userInputMessage": {"content": "user msg"}},
                {"assistantResponseMessage": {"content": "response"}}
            ]
        });
        add_history_cache_points(&mut state, 2);
        assert_eq!(state["history"][0]["cachePoint"], true);
        assert_eq!(state["history"][1]["cachePoint"], true);
        assert!(state["history"][2].get("cachePoint").is_none());
    }

    // ---- New cache tracker tests ----

    #[test]
    fn estimate_approx_tokens_basic() {
        assert_eq!(estimate_approx_tokens(""), 0);
        assert!(estimate_approx_tokens("hello world") > 0);
        assert!(estimate_approx_tokens("你好世界") > 0);
    }

    #[test]
    fn canonicalize_deterministic() {
        let v1 = json!({"b": 1, "a": 2});
        let v2 = json!({"a": 2, "b": 1});
        assert_eq!(canonicalize_cache_value(&v1), canonicalize_cache_value(&v2));
    }

    #[test]
    fn canonicalize_strips_cache_control() {
        let v = json!({"text": "hello", "cache_control": {"type": "ephemeral"}});
        let s = canonicalize_cache_value(&v);
        assert!(!s.contains("cache_control"));
        assert!(s.contains("text"));
    }

    #[test]
    fn tracker_first_request_reports_creation() {
        let tracker = PromptCacheTracker::new();
        let text = "This is a longer test message for cache tracking. ".repeat(200);
        let body = json!({
            "model": "claude-sonnet-4.5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}
            ]}]
        });
        let profile = tracker.build_profile(&body, 5000, "claude-sonnet-4.5").unwrap();
        let usage = tracker.compute("acct1", &profile);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert!(usage.cache_creation_input_tokens > 0);
    }

    #[test]
    fn tracker_second_request_reports_read() {
        let tracker = PromptCacheTracker::new();
        // Use enough text to exceed min_cacheable_tokens (1024)
        let text = "This is a longer test message for cache tracking purposes. ".repeat(200);
        let body = json!({
            "model": "claude-sonnet-4.5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}
            ]}]
        });
        let profile = tracker.build_profile(&body, 5000, "claude-sonnet-4.5").unwrap();

        // First request
        let usage1 = tracker.compute("acct1", &profile);
        assert_eq!(usage1.cache_read_input_tokens, 0);
        assert!(usage1.cache_creation_input_tokens > 0);
        tracker.update("acct1", &profile);

        // Second request (same content should match fingerprint)
        let usage2 = tracker.compute("acct1", &profile);
        assert!(usage2.cache_read_input_tokens > 0, "Expected cache_read > 0, got creation={}, read={}", usage2.cache_creation_input_tokens, usage2.cache_read_input_tokens);
    }

    #[test]
    fn tracker_different_accounts_independent() {
        let tracker = PromptCacheTracker::new();
        let text = "This is a longer test message for cache tracking purposes. ".repeat(200);
        let body = json!({
            "model": "claude-sonnet-4.5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}
            ]}]
        });
        let profile = tracker.build_profile(&body, 5000, "claude-sonnet-4.5").unwrap();

        tracker.compute("acct1", &profile);
        tracker.update("acct1", &profile);

        // Different account should see creation, not read
        let usage2 = tracker.compute("acct2", &profile);
        assert_eq!(usage2.cache_read_input_tokens, 0);
        assert!(usage2.cache_creation_input_tokens > 0);
    }

    #[test]
    fn tracker_no_cache_markers_returns_none() {
        let tracker = PromptCacheTracker::new();
        let body = json!({
            "model": "claude-sonnet-4.5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]
        });
        assert!(tracker.build_profile(&body, 10, "claude-sonnet-4.5").is_none());
    }
}
