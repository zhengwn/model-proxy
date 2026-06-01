//! Multi-pass conversation sanitization pipeline for Kiro API.
//!
//! Ensures conversation history conforms to Kiro's strict invariants:
//! - Only user/assistant roles
//! - Strict user/assistant alternation
//! - No orphaned tool_use without matching tool_result
//! - First message is user, last message is user
//! - Sentinel messages for empty/broken sequences

use serde_json::{json, Value};
use tracing::debug;

/// Sentinel: initial greeting when first user message is missing
const HELLO_SENTINEL: &str = "Hello";
/// Sentinel: continuation when trailing user message is needed
const CONTINUE_SENTINEL: &str = "Continue";
/// Sentinel: acknowledgment between consecutive user messages
const UNDERSTOOD_SENTINEL: &str = "understood";

/// Result of the sanitization pipeline.
#[derive(Debug, Default)]
pub struct SanitizeResult {
    /// Number of synthetic messages inserted
    pub inserted: usize,
    /// Number of messages removed or modified
    pub modified: usize,
    /// Number of orphan tool_results repaired
    pub orphans_repaired: usize,
}

/// Conversation sanitizer with configurable passes.
pub struct ConversationSanitizer {
    pub enforce_boundary_guards: bool,
    pub insert_sentinels: bool,
}

impl Default for ConversationSanitizer {
    fn default() -> Self {
        Self {
            enforce_boundary_guards: true,
            insert_sentinels: true,
        }
    }
}

impl ConversationSanitizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run the full sanitization pipeline on Kiro-format history messages.
    /// Messages should already be in Kiro format (userInputMessage / assistantResponseMessage).
    pub fn sanitize(&self, messages: &mut Vec<Value>) -> SanitizeResult {
        let mut result = SanitizeResult::default();

        if messages.is_empty() {
            return result;
        }

        // Pass 1: Strip empty user messages (after first)
        let before = messages.len();
        self.pass_strip_empty(messages);
        result.modified += before.saturating_sub(messages.len());

        // Pass 2: Boundary guards - ensure first is user
        if self.enforce_boundary_guards {
            if !is_kiro_user(&messages[0]) {
                messages.insert(
                    0,
                    make_kiro_user(HELLO_SENTINEL),
                );
                result.inserted += 1;
            }
        }

        // Pass 3: Enforce strict alternation
        let (inserted, removed) = self.pass_enforce_alternation(messages);
        result.inserted += inserted;
        result.modified += removed;

        // Pass 4: Boundary guards - ensure last is user
        if self.enforce_boundary_guards {
            if let Some(last) = messages.last() {
                if !is_kiro_user(last) {
                    messages.push(make_kiro_user(CONTINUE_SENTINEL));
                    result.inserted += 1;
                }
            }
        }

        // Pass 5: Repair orphaned tool_use (assistant has tool_use but no matching tool_result)
        let orphans = self.pass_repair_orphan_tool_use(messages);
        result.orphans_repaired += orphans;

        debug!(
            inserted = result.inserted,
            modified = result.modified,
            orphans = result.orphans_repaired,
            "会话清洗完成"
        );

        result
    }

    /// Remove empty user messages that have no content and no tool results.
    fn pass_strip_empty(&self, messages: &mut Vec<Value>) {
        let mut seen_first_user = false;
        messages.retain(|msg| {
            if is_kiro_user(msg) {
                if !seen_first_user {
                    seen_first_user = true;
                    return true; // Always keep the first user message
                }
                let user_msg = msg.get("userInputMessage");
                let content = user_msg
                    .and_then(|m| m.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let has_tool_results = user_msg
                    .and_then(|m| m.get("userInputMessageContext"))
                    .and_then(|c| c.get("toolResults"))
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                !content.is_empty() || has_tool_results
            } else {
                true
            }
        });
    }

    /// Enforce strict user/assistant alternation.
    /// Returns (inserted_count, removed_count).
    fn pass_enforce_alternation(&self, messages: &mut Vec<Value>) -> (usize, usize) {
        let mut inserted = 0;
        let mut i = 0;
        while i + 1 < messages.len() {
            let current_is_user = is_kiro_user(&messages[i]);
            let next_is_user = is_kiro_user(&messages[i + 1]);

            if current_is_user && next_is_user {
                // Two consecutive user messages: insert "understood" assistant between
                messages.insert(i + 1, make_kiro_assistant(UNDERSTOOD_SENTINEL));
                inserted += 1;
                i += 2;
            } else if !current_is_user && !next_is_user {
                // Two consecutive assistant messages: insert "Continue" user between
                messages.insert(i + 1, make_kiro_user(CONTINUE_SENTINEL));
                inserted += 1;
                i += 2;
            } else {
                i += 1;
            }
        }
        (inserted, 0)
    }

    /// Repair orphaned tool_use blocks: if assistant has tool_use but the next
    /// user message lacks matching tool_results, inject synthetic error results.
    fn pass_repair_orphan_tool_use(&self, messages: &mut Vec<Value>) -> usize {
        let mut orphans_repaired = 0;

        // Collect tool_use IDs from each assistant message
        for i in 0..messages.len() {
            if is_kiro_assistant(&messages[i]) {
                let tool_use_ids = extract_tool_use_ids(&messages[i]);
                if tool_use_ids.is_empty() {
                    continue;
                }

                // Check if next message is user with matching tool_results
                if i + 1 < messages.len() && is_kiro_user(&messages[i + 1]) {
                    let result_ids = extract_tool_result_ids(&messages[i + 1]);
                    let missing: Vec<&str> = tool_use_ids
                        .iter()
                        .filter(|id| !result_ids.contains(id))
                        .map(|s| s.as_str())
                        .collect();

                    if !missing.is_empty() {
                        // Inject synthetic tool_results into the user message
                        inject_synthetic_tool_results(&mut messages[i + 1], &missing);
                        orphans_repaired += missing.len();
                    }
                } else if i + 1 < messages.len() {
                    // Next message is also assistant (shouldn't happen after alternation pass)
                    // Create a synthetic user message with error results
                    let mut synthetic = make_kiro_user(CONTINUE_SENTINEL);
                    inject_synthetic_tool_results(&mut synthetic, &tool_use_ids.iter().map(|s| s.as_str()).collect::<Vec<_>>());
                    messages.insert(i + 1, synthetic);
                    orphans_repaired += tool_use_ids.len();
                }
            }
        }

        orphans_repaired
    }
}

// ---- Kiro message format helpers ----

fn is_kiro_user(msg: &Value) -> bool {
    msg.get("userInputMessage").is_some()
}

fn is_kiro_assistant(msg: &Value) -> bool {
    msg.get("assistantResponseMessage").is_some()
}

fn make_kiro_user(content: &str) -> Value {
    json!({
        "userInputMessage": {
            "content": content,
            "modelId": "auto",
            "origin": "AI_EDITOR"
        }
    })
}

fn make_kiro_assistant(content: &str) -> Value {
    json!({
        "assistantResponseMessage": {
            "content": content
        }
    })
}

/// Extract tool_use IDs from a Kiro assistant message.
fn extract_tool_use_ids(msg: &Value) -> Vec<String> {
    let mut ids = Vec::new();

    // Check assistantResponseMessage content for tool_use blocks
    if let Some(assistant) = msg.get("assistantResponseMessage") {
        if let Some(content) = assistant.get("content").and_then(|v| v.as_array()) {
            for block in content {
                if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                        ids.push(id.to_string());
                    }
                }
            }
        }
    }
    ids
}

/// Extract tool_result IDs from a Kiro user message.
fn extract_tool_result_ids(msg: &Value) -> Vec<String> {
    let mut ids = Vec::new();

    if let Some(user) = msg.get("userInputMessage") {
        if let Some(context) = user.get("userInputMessageContext") {
            if let Some(results) = context.get("toolResults").and_then(|v| v.as_array()) {
                for result in results {
                    if let Some(id) = result.get("toolUseId").and_then(|v| v.as_str()) {
                        ids.push(id.to_string());
                    }
                }
            }
        }
    }
    ids
}

/// Inject synthetic error tool_results into a Kiro user message.
fn inject_synthetic_tool_results(msg: &mut Value, tool_use_ids: &[&str]) {
    let user_msg = msg
        .get_mut("userInputMessage")
        .expect("expected userInputMessage");

    // Ensure userInputMessageContext exists and has toolResults array
    if user_msg.get("userInputMessageContext").is_none() {
        user_msg["userInputMessageContext"] = json!({});
    }
    let context = user_msg.get_mut("userInputMessageContext").unwrap();
    if context.get("toolResults").is_none() {
        context["toolResults"] = json!([]);
    }
    let results = context.get_mut("toolResults").unwrap();
    let results_arr = results.as_array_mut().expect("expected array");

    for id in tool_use_ids {
        results_arr.push(json!({
            "toolUseId": id,
            "status": "error",
            "content": "Tool execution failed - no result received"
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> Value {
        make_kiro_user(content)
    }

    fn assistant(content: &str) -> Value {
        make_kiro_assistant(content)
    }

    #[test]
    fn sanitize_empty() {
        let sanitizer = ConversationSanitizer::new();
        let mut msgs = vec![];
        let result = sanitizer.sanitize(&mut msgs);
        assert_eq!(result.inserted, 0);
        assert!(msgs.is_empty());
    }

    #[test]
    fn sanitize_valid_conversation() {
        let sanitizer = ConversationSanitizer::new();
        let mut msgs = vec![user("hi"), assistant("hello"), user("bye")];
        let result = sanitizer.sanitize(&mut msgs);
        // Already valid: user, assistant, user
        assert_eq!(result.inserted, 0);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn sanitize_fixes_leading_assistant() {
        let sanitizer = ConversationSanitizer::new();
        let mut msgs = vec![assistant("orphan"), user("hi")];
        let result = sanitizer.sanitize(&mut msgs);
        // Should prepend Hello sentinel
        assert!(result.inserted >= 1);
        assert!(is_kiro_user(&msgs[0]));
        assert_eq!(
            msgs[0]["userInputMessage"]["content"].as_str().unwrap(),
            HELLO_SENTINEL
        );
    }

    #[test]
    fn sanitize_fixes_trailing_assistant() {
        let sanitizer = ConversationSanitizer::new();
        let mut msgs = vec![user("hi"), assistant("hello")];
        let result = sanitizer.sanitize(&mut msgs);
        // Should append Continue sentinel
        assert!(result.inserted >= 1);
        assert!(is_kiro_user(msgs.last().unwrap()));
    }

    #[test]
    fn sanitize_fixes_consecutive_users() {
        let sanitizer = ConversationSanitizer::new();
        let mut msgs = vec![user("a"), user("b"), assistant("c")];
        let result = sanitizer.sanitize(&mut msgs);
        // Should insert understood between consecutive users
        assert!(result.inserted >= 1);
        // Verify alternation
        for i in 0..msgs.len() - 1 {
            let current_user = is_kiro_user(&msgs[i]);
            let next_user = is_kiro_user(&msgs[i + 1]);
            assert_ne!(current_user, next_user, "Messages at {} and {} should alternate", i, i + 1);
        }
    }

    #[test]
    fn sanitize_strips_empty_messages() {
        let sanitizer = ConversationSanitizer::new();
        let mut msgs = vec![user("hi"), user(""), assistant("ok")];
        let result = sanitizer.sanitize(&mut msgs);
        // Empty user message should be stripped
        assert!(result.modified >= 1);
    }
}
