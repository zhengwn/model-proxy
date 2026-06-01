//! Context compression: token estimation, compression-needed checks,
//! and tool-boundary-aware truncation.
//!
//! Uses a simple HashMap cache (no LRU) for storing compressed summaries.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60); // 30 minutes

/// Cached summary for a compression key.
#[derive(Debug, Clone)]
pub struct CachedSummary {
    pub summary: String,
    pub created_at: Instant,
    pub ttl: Duration,
}

/// Simple HashMap-based compression cache.
#[derive(Clone)]
pub struct CompressionCache {
    inner: Arc<RwLock<HashMap<String, CachedSummary>>>,
}

impl CompressionCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get a cached summary if it exists and hasn't expired.
    pub fn get(&self, key: &str) -> Option<String> {
        let inner = self.inner.read().unwrap();
        inner.get(key).and_then(|entry| {
            if entry.created_at.elapsed() < entry.ttl {
                Some(entry.summary.clone())
            } else {
                None
            }
        })
    }

    /// Store a summary with the default TTL.
    pub fn put(&self, key: &str, summary: String) {
        self.put_with_ttl(key, summary, DEFAULT_TTL);
    }

    /// Store a summary with a custom TTL.
    pub fn put_with_ttl(&self, key: &str, summary: String, ttl: Duration) {
        let mut inner = self.inner.write().unwrap();
        inner.insert(
            key.to_string(),
            CachedSummary {
                summary,
                created_at: Instant::now(),
                ttl,
            },
        );
    }

    /// Remove expired entries from the cache.
    pub fn cleanup(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.retain(|_, entry| entry.created_at.elapsed() < entry.ttl);
    }

    /// Number of entries in the cache (including expired).
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }
}

impl Default for CompressionCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Estimate the number of tokens in a text string.
///
/// Uses a rough heuristic: ~4 characters per token for English text,
/// ~2 characters per token for CJK-heavy text. Falls back to ceil(len/4).
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }

    let byte_len = text.len();
    // Count CJK characters (3-byte UTF-8 sequences starting with 0xE0-0xEF)
    let cjk_count = text
        .chars()
        .filter(|c| {
            let cp = *c as u32;
            // CJK Unified Ideographs
            (0x4E00..=0x9FFF).contains(&cp)
                // CJK Extension A
                || (0x3400..=0x4DBF).contains(&cp)
                // CJK Compatibility
                || (0xF900..=0xFAFF).contains(&cp)
        })
        .count();

    let cjk_bytes = cjk_count * 3;
    let non_cjk_bytes = byte_len.saturating_sub(cjk_bytes);

    // ~2 chars/token for CJK, ~4 chars/token for other text
    let cjk_tokens = (cjk_count + 1) / 2;
    let non_cjk_tokens = (non_cjk_bytes + 3) / 4;

    cjk_tokens + non_cjk_tokens
}

/// Check whether a list of messages needs compression based on a token threshold.
pub fn needs_compression(messages: &[Value], threshold: usize) -> bool {
    let total_tokens: usize = messages
        .iter()
        .filter_map(|m| m.get("content"))
        .map(|content| {
            let text = match content {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };
            estimate_tokens(&text)
        })
        .sum();

    total_tokens > threshold
}

/// Truncate messages at tool_use/tool_result boundaries.
///
/// Preserves paired tool_use and tool_result messages: if the last kept message
/// is a tool_use, also keeps the corresponding tool_result.
///
/// Returns the last `keep_count` messages with tool pairs intact.
pub fn truncate_at_tool_boundary(messages: &[Value], keep_count: usize) -> Vec<Value> {
    if messages.len() <= keep_count {
        return messages.to_vec();
    }

    let start = messages.len() - keep_count;
    let mut result: Vec<Value> = messages[start..].to_vec();

    // If the first kept message references a tool_result, we need the corresponding
    // tool_use. Walk backwards through the original messages to find it.
    if let Some(first) = result.first() {
        if is_tool_result(first) {
            let tool_use_id = get_tool_use_id(first);
            if let Some(tool_use_msg) = tool_use_id.and_then(|id| find_tool_use(messages, &id, start))
            {
                result.insert(0, tool_use_msg);
            }
        }
    }

    // If the last kept message is a tool_use, ensure the corresponding tool_result follows.
    if let Some(last) = result.last() {
        if is_tool_use(last) {
            let tool_use_id = get_tool_use_id(last);
            if let Some(tool_result_msg) = tool_use_id.and_then(|id| {
                find_tool_result_in(messages, &id, messages.len() - 1)
            }) {
                result.push(tool_result_msg);
            }
        }
    }

    result
}

fn is_tool_use(msg: &Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
        && msg
            .get("content")
            .and_then(|c| c.as_array())
            .map(|arr| arr.iter().any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_use")))
            .unwrap_or(false)
}

fn is_tool_result(msg: &Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("user")
        && msg
            .get("content")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            })
            .unwrap_or(false)
}

fn get_tool_use_id(msg: &Value) -> Option<String> {
    // For tool_use messages (assistant)
    if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
        for part in arr {
            if part.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(id) = part.get("id").and_then(|i| i.as_str()) {
                    return Some(id.to_string());
                }
            }
            if part.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                if let Some(id) = part.get("tool_use_id").and_then(|i| i.as_str()) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

/// Find a tool_use message by its id in messages[0..end].
fn find_tool_use(messages: &[Value], tool_use_id: &str, end: usize) -> Option<Value> {
    messages[..end].iter().rev().find(|msg| {
        msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
            && msg
                .get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter().any(|p| {
                        p.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                            && p.get("id").and_then(|i| i.as_str()) == Some(tool_use_id)
                    })
                })
                .unwrap_or(false)
    }).cloned()
}

/// Find a tool_result message by its tool_use_id in messages[start_from..].
fn find_tool_result_in(messages: &[Value], tool_use_id: &str, start_from: usize) -> Option<Value> {
    messages[start_from..].iter().find(|msg| {
        msg.get("role").and_then(|r| r.as_str()) == Some("user")
            && msg
                .get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter().any(|p| {
                        p.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                            && p.get("tool_use_id").and_then(|i| i.as_str()) == Some(tool_use_id)
                    })
                })
                .unwrap_or(false)
    }).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_put_and_get() {
        let cache = CompressionCache::new();
        cache.put("key1", "summary text".to_string());
        assert_eq!(cache.get("key1"), Some("summary text".to_string()));
    }

    #[test]
    fn cache_get_missing_key() {
        let cache = CompressionCache::new();
        assert_eq!(cache.get("nonexistent"), None);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_english() {
        // "hello world" is 11 chars, ~3 tokens
        let tokens = estimate_tokens("hello world");
        assert!(tokens >= 2 && tokens <= 4);
    }

    #[test]
    fn needs_compression_small_messages() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        assert!(!needs_compression(&messages, 1000));
    }

    #[test]
    fn truncate_preserves_count() {
        let messages: Vec<Value> = (0..10)
            .map(|i| json!({"role": "user", "content": format!("msg{}", i)}))
            .collect();
        let result = truncate_at_tool_boundary(&messages, 5);
        assert_eq!(result.len(), 5);
    }
}
