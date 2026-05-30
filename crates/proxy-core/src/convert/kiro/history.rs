//! History management for Kiro conversations.
//!
//! Provides strategies for handling long conversation histories:
//! - Auto-truncate: keep last N messages under M characters
//! - Error-retry truncation: progressively reduce on content-length errors
//! - Pre-estimate: check size before sending

use serde_json::{json, Value};
use tracing::{debug, info, warn};

/// Maximum number of history messages to keep by default.
const DEFAULT_MAX_MESSAGES: usize = 30;

/// Maximum total character count for history.
const DEFAULT_MAX_CHARS: usize = 150_000;

/// Truncation strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationStrategy {
    /// Keep last N messages, no size check
    AutoTruncate,
    /// On content-length error, progressively reduce by 30%
    ErrorRetry,
    /// Pre-check size, truncate if over threshold
    PreEstimate,
    /// No truncation
    None,
}

/// Configuration for history management.
pub struct HistoryConfig {
    pub strategy: TruncationStrategy,
    pub max_messages: usize,
    pub max_chars: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            strategy: TruncationStrategy::AutoTruncate,
            max_messages: DEFAULT_MAX_MESSAGES,
            max_chars: DEFAULT_MAX_CHARS,
        }
    }
}

/// Manage conversation history with truncation strategies.
pub struct HistoryManager {
    config: HistoryConfig,
}

impl HistoryManager {
    pub fn new(config: HistoryConfig) -> Self {
        Self { config }
    }

    /// Process history messages according to the configured strategy.
    /// Returns the potentially trimmed history array.
    pub fn process_history(&self, history: &mut Vec<Value>) {
        match self.config.strategy {
            TruncationStrategy::AutoTruncate => self.auto_truncate(history),
            TruncationStrategy::PreEstimate => self.pre_estimate(history),
            TruncationStrategy::ErrorRetry => {
                // Error-retry is triggered externally; just do auto-truncate as baseline
                self.auto_truncate(history);
            }
            TruncationStrategy::None => {}
        }
    }

    /// Truncate history to fit within max_messages and max_chars limits.
    /// Preserves user/assistant pairing by removing pairs from the beginning.
    fn auto_truncate(&self, history: &mut Vec<Value>) {
        // Message count limit
        while history.len() > self.config.max_messages {
            // Remove pairs (2 at a time) from the beginning
            history.remove(0);
            if !history.is_empty() {
                history.remove(0);
            }
        }

        // Character count limit
        loop {
            let total_chars: usize = history
                .iter()
                .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
                .sum();

            if total_chars <= self.config.max_chars || history.len() <= 2 {
                break;
            }

            // Remove oldest pair
            history.remove(0);
            if !history.is_empty() {
                history.remove(0);
            }

            debug!(
                remaining = history.len(),
                total_chars, "History 截断进行中"
            );
        }

        // Ensure alternating roles after truncation
        ensure_alternating(history);
    }

    /// Pre-estimate payload size and truncate if needed.
    fn pre_estimate(&self, history: &mut Vec<Value>) {
        let total_chars: usize = history
            .iter()
            .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
            .sum();

        if total_chars > self.config.max_chars {
            info!(
                total_chars,
                max_chars = self.config.max_chars,
                "History 预估超限，开始截断"
            );
            self.auto_truncate(history);
        }
    }

    /// Progressive truncation for error-retry: reduce by a percentage.
    pub fn truncate_for_retry(&self, history: &mut Vec<Value>, reduction_pct: f64) {
        let target = (history.len() as f64 * (1.0 - reduction_pct)) as usize;
        let target = target.max(2); // Keep at least 2 entries

        while history.len() > target {
            history.remove(0);
            if !history.is_empty() {
                history.remove(0);
            }
        }

        ensure_alternating(history);

        warn!(
            target_len = target,
            actual_len = history.len(),
            "History 错误重试截断"
        );
    }

    /// Build a smart summary of old messages for long conversations.
    /// Returns a summary text that can replace old history entries.
    pub fn build_summary_text(old_messages: &[Value]) -> String {
        let mut summary_parts = Vec::new();
        summary_parts.push("[Conversation Summary]".to_string());

        for msg in old_messages {
            let role = if msg.get("userInputMessage").is_some() || msg.get("role").and_then(|v| v.as_str()) == Some("user") {
                "User"
            } else {
                "Assistant"
            };

            let text = extract_text_from_kiro_message(msg);
            if !text.is_empty() {
                // Take first 200 chars as summary snippet
                let snippet: String = text.chars().take(200).collect();
                if text.len() > 200 {
                    summary_parts.push(format!("{}: {}...", role, snippet));
                } else {
                    summary_parts.push(format!("{}: {}", role, snippet));
                }
            }
        }

        summary_parts.join("\n")
    }

    /// Replace old history with a summary + recent messages.
    pub fn apply_smart_summary(
        &self,
        history: &mut Vec<Value>,
        keep_recent: usize,
    ) {
        if history.len() <= keep_recent + 2 {
            return; // Not enough messages to summarize
        }

        let split_point = history.len() - keep_recent;
        let old_messages = &history[..split_point];
        let summary = Self::build_summary_text(old_messages);

        // Replace old messages with summary
        let recent: Vec<Value> = history[split_point..].to_vec();
        history.clear();

        // Add summary as synthetic user + assistant pair
        history.push(json!({"userInputMessage": {"content": summary, "modelId": "auto", "origin": "AI_EDITOR"}}));
        history.push(json!({"assistantResponseMessage": {"content": "I understand the conversation context."}}));

        // Add recent messages
        history.extend(recent);

        ensure_alternating(history);

        info!(
            summarized = split_point,
            kept = keep_recent,
            "Smart Summary 已应用"
        );
    }
}

/// Ensure history starts with a user message and has alternating roles.
fn ensure_alternating(history: &mut Vec<Value>) {
    if history.is_empty() {
        return;
    }

    // Remove leading assistant messages
    while history.first().map(|m| is_assistant(m)).unwrap_or(false) {
        history.remove(0);
    }

    // Remove trailing user messages that would leave orphan
    // (this is handled by the request converter, so we don't do it here)

    // Insert synthetic assistant messages between consecutive user messages
    let mut i = 0;
    while i + 1 < history.len() {
        if is_user(&history[i]) && is_user(&history[i + 1]) {
            history.insert(
                i + 1,
                json!({"assistantResponseMessage": {"content": "OK"}}),
            );
        }
        i += 1;
    }
}

fn is_user(msg: &Value) -> bool {
    msg.get("userInputMessage").is_some()
        || msg.get("role").and_then(|v| v.as_str()) == Some("user")
}

fn is_assistant(msg: &Value) -> bool {
    msg.get("assistantResponseMessage").is_some()
        || msg.get("role").and_then(|v| v.as_str()) == Some("assistant")
}

/// Extract text content from a Kiro-format message (userInputMessage or assistantResponseMessage).
fn extract_text_from_kiro_message(msg: &Value) -> String {
    if let Some(user_msg) = msg.get("userInputMessage") {
        if let Some(content) = user_msg.get("content").and_then(|v| v.as_str()) {
            return content.to_string();
        }
    }
    if let Some(assistant_msg) = msg.get("assistantResponseMessage") {
        if let Some(content) = assistant_msg.get("content").and_then(|v| v.as_str()) {
            return content.to_string();
        }
    }
    // Try Anthropic/OpenAI format
    if let Some(content) = msg.get("content") {
        if let Some(s) = content.as_str() {
            return s.to_string();
        }
        if let Some(arr) = content.as_array() {
            return arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_user(content: &str) -> Value {
        json!({"userInputMessage": {"content": content}})
    }

    fn make_assistant(content: &str) -> Value {
        json!({"assistantResponseMessage": {"content": content}})
    }

    #[test]
    fn auto_truncate_preserves_pairs() {
        let mgr = HistoryManager::new(HistoryConfig {
            max_messages: 4,
            max_chars: 100_000,
            strategy: TruncationStrategy::AutoTruncate,
        });

        let mut history = vec![
            make_user("old1"),
            make_assistant("old2"),
            make_user("new1"),
            make_assistant("new2"),
            make_user("new3"),
            make_assistant("new4"),
        ];

        mgr.process_history(&mut history);
        assert!(history.len() <= 4);
    }

    #[test]
    fn ensure_alternating_inserts_assistant() {
        let mut history = vec![
            make_user("a"),
            make_user("b"),
        ];
        ensure_alternating(&mut history);
        assert_eq!(history.len(), 3);
        assert!(is_assistant(&history[1]));
    }

    #[test]
    fn ensure_alternating_removes_leading_assistant() {
        let mut history = vec![
            make_assistant("orphan"),
            make_user("a"),
            make_assistant("b"),
        ];
        ensure_alternating(&mut history);
        assert!(is_user(&history[0]));
    }

    #[test]
    fn truncate_for_retry() {
        let mgr = HistoryManager::new(HistoryConfig::default());
        let mut history = vec![
            make_user("1"), make_assistant("2"),
            make_user("3"), make_assistant("4"),
            make_user("5"), make_assistant("6"),
            make_user("7"), make_assistant("8"),
            make_user("9"), make_assistant("10"),
        ];

        mgr.truncate_for_retry(&mut history, 0.5);
        assert!(history.len() <= 6); // 50% reduction, min 2
    }
}
