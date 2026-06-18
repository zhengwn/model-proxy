//! LLM Smart Summary for CONTENT_TOO_LONG retry.
//!
//! When Kiro API rejects a request with CONTENT_LENGTH_EXCEEDS, this module
//! generates a summary of old conversation history using a fast Haiku model,
//! replaces the old messages with the summary, and retries the request.
//!
//! Falls back to tiered truncation if the summary API call fails.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use super::endpoint::{build_endpoint_headers, build_endpoint_url, KIRO_ENDPOINTS};
use super::eventstream::{self, Event, EventStreamDecoder};
use super::truncation::{truncate_kiro_payload_history, TRUNCATION_TIERS};
use crate::error::{AppError, Result};

// ---- Constants ----

/// Model used for summary generation (fast + cheap)
const SUMMARY_MODEL: &str = "claude-haiku-4.5";

/// Timeout for the summary API call
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum characters for formatted input to the summary prompt
const INPUT_CAP: usize = 10_000;

/// Maximum characters for a single message in the formatted input
const MSG_CHAR_LIMIT: usize = 500;

/// Maximum length of the generated summary
const SUMMARY_MAX_LENGTH: usize = 2000;

/// Number of recent messages to keep unsummarized
const KEEP_RECENT: usize = 6;

/// Minimum history entries before summary is worthwhile
const MIN_HISTORY_FOR_SUMMARY: usize = 8;

/// Cache TTL
const CACHE_TTL: Duration = Duration::from_secs(180);

/// Maximum cache entries
const CACHE_MAX_ENTRIES: usize = 64;

// ---- Summary Cache ----

/// Cached summary entry.
struct SummaryEntry {
    summary: String,
    created_at: Instant,
    old_message_count: usize,
}

/// Thread-safe LRU summary cache keyed by a hash of old messages.
pub struct SummaryCache {
    entries: Mutex<HashMap<String, SummaryEntry>>,
}

impl SummaryCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Try to get a cached summary. Returns None if expired or delta too large.
    fn get(&self, key: &str, current_old_count: usize) -> Option<String> {
        let entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get(key) {
            if entry.created_at.elapsed() < CACHE_TTL {
                // Only reuse if the old messages haven't changed significantly
                let delta = current_old_count.abs_diff(entry.old_message_count);
                if delta < 3 {
                    return Some(entry.summary.clone());
                }
            }
        }
        None
    }

    /// Store a summary in the cache.
    fn put(&self, key: String, summary: String, old_message_count: usize) {
        let mut entries = self.entries.lock().unwrap();
        // Evict oldest if at capacity
        if entries.len() >= CACHE_MAX_ENTRIES {
            if let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.created_at)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest_key);
            }
        }
        entries.insert(
            key,
            SummaryEntry {
                summary,
                created_at: Instant::now(),
                old_message_count,
            },
        );
    }
}

impl Default for SummaryCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---- History Formatting ----

/// Format history messages into a readable text for the summary prompt.
/// Each message becomes `[role]: text` with tool call annotations.
fn format_history_for_summary(history: &[Value]) -> String {
    let mut lines = Vec::new();
    let mut total_chars = 0;

    for entry in history {
        let (role, content, tool_info) = if let Some(user_msg) = entry.get("userInputMessage") {
            let text = user_msg
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Check for tool results
            let tool_result_count = user_msg
                .get("userInputMessageContext")
                .and_then(|c| c.get("toolResults"))
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let tool_info = if tool_result_count > 0 {
                format!(" [{} tool results]", tool_result_count)
            } else {
                String::new()
            };
            ("user", text, tool_info)
        } else if let Some(assistant_msg) = entry.get("assistantResponseMessage") {
            let text = assistant_msg
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Extract tool_use names if present
            let tool_names: Vec<String> = assistant_msg
                .get("content")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                        .filter_map(|b| b.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let tool_info = if !tool_names.is_empty() {
                format!(" [tools: {}]", tool_names.join(", "))
            } else {
                String::new()
            };
            ("assistant", text, tool_info)
        } else {
            continue;
        };

        let truncated = if content.chars().count() > MSG_CHAR_LIMIT {
            let truncated: String = content.chars().take(MSG_CHAR_LIMIT).collect();
            format!("{}...", truncated)
        } else {
            content.to_string()
        };

        let line = format!("[{}]{}: {}", role, tool_info, truncated);
        if total_chars + line.len() > INPUT_CAP {
            break;
        }
        total_chars += line.len();
        lines.push(line);
    }

    lines.join("\n")
}

/// Build the summary prompt in Chinese (matching KiroProxy's approach).
fn build_summary_prompt(formatted: &str) -> String {
    format!(
        "请简洁地总结以下对话历史的关键信息，按以下结构输出：\n\
         1. **目标**：用户的主要目标和需求\n\
         2. **操作**：已完成的重要操作（特别是工具调用）\n\
         3. **状态**：当前的工作进展和上下文\n\
         4. **关键决策**：做出的重要决定\n\n\
         对话历史：\n\
         {}\n\n\
         请用中文输出摘要，控制在 {} 字符以内：",
        formatted, SUMMARY_MAX_LENGTH
    )
}

/// Generate a cache key from old messages (hash of first 3 message contents).
fn cache_key_from_messages(old_messages: &[Value]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    let mut count = 0;
    for msg in old_messages.iter().take(3) {
        if let Some(content) = msg
            .pointer("/userInputMessage/content")
            .or_else(|| msg.pointer("/assistantResponseMessage/content"))
            .and_then(|v| v.as_str())
        {
            // Hash first 200 chars of each message
            let prefix: String = content.chars().take(200).collect();
            prefix.hash(&mut hasher);
            count += 1;
        }
    }
    old_messages.len().hash(&mut hasher);
    format!("{:016x}_{}", hasher.finish(), count)
}

// ---- Kiro API Summary Call ----

/// Call Kiro API with Haiku to generate a summary.
/// Uses the first (default) Kiro endpoint with the provided auth token.
async fn call_kiro_for_summary(
    prompt: &str,
    token: &str,
    amz_user_agent: &str,
    user_agent: &str,
    region: &str,
    client: &reqwest::Client,
) -> Result<String> {
    let endpoint = &KIRO_ENDPOINTS[0]; // "kiro" endpoint
    let url = build_endpoint_url(endpoint, region);
    let headers = build_endpoint_headers(endpoint, token, amz_user_agent, user_agent);

    // Build minimal Kiro request: just the prompt, no tools, no history
    let payload = json!({
        "conversationState": {
            "conversationId": uuid_simple(),
            "agentContinuationId": uuid_simple(),
            "agentTaskType": "vibe",
            "chatTriggerType": "MANUAL",
            "currentMessage": {
                "userInputMessage": {
                    "content": prompt,
                    "modelId": SUMMARY_MODEL,
                    "origin": "AI_EDITOR"
                }
            }
        }
    });

    let payload_bytes = serde_json::to_vec(&payload)?;

    debug!(
        model = SUMMARY_MODEL,
        prompt_len = prompt.len(),
        "调用 Kiro Haiku 生成对话摘要"
    );

    // Build header map
    let mut header_map = reqwest::header::HeaderMap::new();
    for (key, value) in &headers {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            header_map.insert(name, val);
        }
    }

    let resp = client
        .post(&url)
        .headers(header_map)
        .body(payload_bytes)
        .timeout(SUMMARY_TIMEOUT)
        .send()
        .await
        .map_err(|e| AppError::Request(format!("Summary API 请求失败: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        warn!(status, body_len = body.len(), "Summary API 返回非 200");
        return Err(AppError::Request(format!(
            "Summary API 返回 {}",
            status
        )));
    }

    let body_bytes = resp
        .bytes()
        .await
        .map_err(|e| AppError::Request(format!("Summary API 读取响应失败: {}", e)))?;

    // Try binary EventStream parsing first
    let mut decoder = EventStreamDecoder::new();
    if decoder.feed(&body_bytes).is_ok() {
        let mut text_parts = Vec::new();
        loop {
            match decoder.decode() {
                Ok(Some(frame)) => {
                    if let Ok(Event::AssistantResponse { content }) = Event::from_frame(&frame) {
                        text_parts.push(content);
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    if decoder.is_stopped() {
                        break;
                    }
                }
            }
        }
        if !text_parts.is_empty() {
            let summary = text_parts.join("");
            info!(summary_len = summary.len(), "摘要生成成功 (binary)");
            return Ok(summary);
        }
    }

    // Fallback: try text JSON parsing (for corrupted binary framing)
    let fallback_events = eventstream::try_parse_text_events(&body_bytes);
    for event in fallback_events {
        if let Event::AssistantResponse { content } = event {
            if !content.is_empty() {
                info!(summary_len = content.len(), "摘要生成成功 (text fallback)");
                return Ok(content);
            }
        }
    }

    Err(AppError::Request(
        "Summary API 返回了无法解析的响应".to_string(),
    ))
}

/// Simple UUID-like string for request IDs (no external crate needed).
fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (t >> 96) as u32,
        (t >> 80) as u16,
        (t >> 64) & 0xFFF,
        (t >> 48) & 0xFFFF,
        t & 0xFFFFFFFFFFFF
    )
}

// ---- Public API ----

/// Summarize old history and replace it in the Kiro payload.
///
/// Returns `Ok(true)` if summary was applied and the payload was modified,
/// `Ok(false)` if history was too short to summarize,
/// `Err(...)` if the summary API call failed.
pub async fn summarize_and_replace_history(
    payload: &mut Value,
    token: &str,
    amz_user_agent: &str,
    user_agent: &str,
    region: &str,
    client: &reqwest::Client,
    cache: &SummaryCache,
) -> Result<bool> {
    let history = match payload
        .pointer_mut("/conversationState/history")
        .and_then(|v| v.as_array_mut())
    {
        Some(h) if h.len() >= MIN_HISTORY_FOR_SUMMARY => h,
        _ => return Ok(false),
    };

    let total_entries = history.len();
    let split_point = total_entries.saturating_sub(KEEP_RECENT);
    if split_point == 0 {
        return Ok(false);
    }

    // Split history into old (to summarize) and recent (to keep)
    let old_messages: Vec<Value> = history[..split_point].to_vec();
    let recent_messages: Vec<Value> = history[split_point..].to_vec();

    // Check cache
    let key = cache_key_from_messages(&old_messages);
    let summary = if let Some(cached) = cache.get(&key, old_messages.len()) {
        debug!("使用缓存的摘要");
        cached
    } else {
        // Format old messages and call Kiro Haiku
        let formatted = format_history_for_summary(&old_messages);
        let prompt = build_summary_prompt(&formatted);
        match call_kiro_for_summary(&prompt, token, amz_user_agent, user_agent, region, client)
            .await
        {
            Ok(s) => {
                cache.put(key, s.clone(), old_messages.len());
                s
            }
            Err(e) => {
                warn!(error = %e, "摘要生成失败，将使用截断回退");
                return Err(e);
            }
        }
    };

    // Build summary history pair
    let summary_user = json!({
        "userInputMessage": {
            "content": format!("[Earlier conversation summary]\n{}\n\n[Continuing from recent messages...]", summary),
            "modelId": "auto",
            "origin": "AI_EDITOR"
        }
    });
    let summary_assistant = json!({
        "assistantResponseMessage": {
            "content": "I understand the context. Let's continue."
        }
    });

    // Replace history
    let history_mut = payload
        .pointer_mut("/conversationState/history")
        .and_then(|v| v.as_array_mut())
        .unwrap();
    history_mut.clear();
    history_mut.push(summary_user);
    history_mut.push(summary_assistant);

    // Add recent messages, ensuring first entry is a user message
    for msg in recent_messages {
        let is_user = msg.get("userInputMessage").is_some();
        if history_mut.is_empty() && !is_user {
            continue; // skip leading assistant messages
        }
        history_mut.push(msg);
    }

    info!(
        old_entries = split_point,
        kept_entries = history_mut.len() - 2, // minus summary pair
        summary_len = summary.len(),
        "Smart Summary 已应用：{} 条旧消息 → 摘要 + {} 条近期消息",
        split_point,
        history_mut.len() - 2
    );

    Ok(true)
}

/// Apply tiered truncation to the Kiro payload as a fallback.
/// `tier` indexes into `TRUNCATION_TIERS` (0=50%, 1=25%, 2=0%).
pub fn apply_tiered_truncation(payload: &mut Value, tier: usize) {
    if tier < TRUNCATION_TIERS.len() {
        let fraction = TRUNCATION_TIERS[tier];
        truncate_kiro_payload_history(payload, fraction);
        warn!(
            tier,
            fraction, "Tiered truncation 已应用 (Smart Summary 回退)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_history_basic() {
        let history = vec![
            json!({"userInputMessage": {"content": "Hello, help me write code"}}),
            json!({"assistantResponseMessage": {"content": "Sure, what language?"}}),
            json!({"userInputMessage": {"content": "Rust please"}}),
        ];
        let formatted = format_history_for_summary(&history);
        assert!(formatted.contains("[user]"));
        assert!(formatted.contains("[assistant]"));
        assert!(formatted.contains("Hello, help me write code"));
        assert!(formatted.contains("Rust please"));
    }

    #[test]
    fn format_history_truncates_long_messages() {
        let long_text = "a".repeat(1000);
        let history = vec![json!({"userInputMessage": {"content": long_text}})];
        let formatted = format_history_for_summary(&history);
        // Should be truncated to MSG_CHAR_LIMIT + "..."
        assert!(formatted.len() < 1000);
        assert!(formatted.contains("..."));
    }

    #[test]
    fn format_history_caps_total() {
        // Create many messages that would exceed INPUT_CAP
        let mut history = Vec::new();
        for i in 0..100 {
            history.push(json!({"userInputMessage": {"content": format!("Message {} with some content here", i)}}));
        }
        let formatted = format_history_for_summary(&history);
        assert!(formatted.len() <= INPUT_CAP + 200); // small overshoot ok from last message
    }

    #[test]
    fn build_summary_prompt_format() {
        let prompt = build_summary_prompt("[user]: test");
        assert!(prompt.contains("请简洁地总结"));
        assert!(prompt.contains("[user]: test"));
        assert!(prompt.contains("关键决策"));
        assert!(prompt.contains(&SUMMARY_MAX_LENGTH.to_string()));
    }

    #[test]
    fn cache_key_deterministic() {
        let messages = vec![
            json!({"userInputMessage": {"content": "hello"}}),
            json!({"assistantResponseMessage": {"content": "hi there"}}),
        ];
        let key1 = cache_key_from_messages(&messages);
        let key2 = cache_key_from_messages(&messages);
        assert_eq!(key1, key2);
    }

    #[test]
    fn cache_key_different_for_different_messages() {
        let msgs1 = vec![json!({"userInputMessage": {"content": "hello"}})];
        let msgs2 = vec![json!({"userInputMessage": {"content": "goodbye"}})];
        assert_ne!(cache_key_from_messages(&msgs1), cache_key_from_messages(&msgs2));
    }

    #[test]
    fn summary_cache_ttl() {
        let cache = SummaryCache::new();
        cache.put("key1".to_string(), "summary".to_string(), 10);
        assert!(cache.get("key1", 10).is_some());
        // Different old_count within delta should still hit
        assert!(cache.get("key1", 12).is_some());
        // Large delta should miss
        assert!(cache.get("key1", 20).is_none());
    }

    #[test]
    fn summary_cache_nonexistent() {
        let cache = SummaryCache::new();
        assert!(cache.get("missing", 0).is_none());
    }
}
