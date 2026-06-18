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
        if self.enforce_boundary_guards
            && !is_kiro_user(&messages[0]) {
                messages.insert(
                    0,
                    make_kiro_user(HELLO_SENTINEL),
                );
                result.inserted += 1;
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

// ---- Deep / Aggressive sanitize for 400 error recovery ----

/// Deep sanitize: fill empty content, deduplicate tool results, repair orphans.
/// Returns true if any modifications were made.
pub fn deep_sanitize(messages: &mut Vec<Value>) -> bool {
    let mut modified = false;

    for msg in messages.iter_mut() {
        // Fill empty assistant content with placeholder
        if let Some(assistant) = msg.get_mut("assistantResponseMessage") {
            let content = assistant.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                assistant["content"] = json!("Continue");
                modified = true;
            }
        }

        // Fill empty user content (but keep messages with tool results)
        if let Some(user) = msg.get_mut("userInputMessage") {
            let has_tool_results = user
                .get("userInputMessageContext")
                .and_then(|c| c.get("toolResults"))
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            let content = user.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() && !has_tool_results {
                user["content"] = json!("Continue");
                modified = true;
            }
        }

        // Deduplicate tool_results by toolUseId (keep last)
        if let Some(user) = msg.get_mut("userInputMessage") {
            if let Some(results) = user
                .pointer_mut("/userInputMessageContext/toolResults")
                .and_then(|v| v.as_array_mut())
            {
                let original_len = results.len();
                let mut seen_ids = std::collections::HashSet::new();
                // Iterate in reverse to keep the last occurrence
                let mut deduped: Vec<Value> = Vec::new();
                for result in results.iter().rev() {
                    let id = result.get("toolUseId").and_then(|v| v.as_str()).unwrap_or("");
                    if id.is_empty() || seen_ids.insert(id.to_string()) {
                        deduped.push(result.clone());
                    }
                }
                deduped.reverse();
                if deduped.len() != original_len {
                    *results = deduped;
                    modified = true;
                }
            }
        }
    }

    // Re-run the standard sanitizer for orphan repair and alternation
    let sanitizer = ConversationSanitizer {
        enforce_boundary_guards: true,
        insert_sentinels: true,
    };
    let result = sanitizer.sanitize(messages);
    if result.inserted > 0 || result.orphans_repaired > 0 {
        modified = true;
    }

    debug!(modified, "deep_sanitize 完成");
    modified
}

/// Aggressive sanitize: strip ALL tool history, keep only text conversation.
/// This is the last-resort fix when Kiro returns 400 for malformed tool data.
/// Returns true if any modifications were made.
pub fn aggressive_sanitize(messages: &mut Vec<Value>) -> bool {
    let mut modified = false;

    for msg in messages.iter_mut() {
        // Strip tool_use blocks from assistant messages, keep text only
        if let Some(assistant) = msg.get_mut("assistantResponseMessage") {
            if let Some(content) = assistant.get("content").and_then(|v| v.as_array()) {
                let text_only: Vec<Value> = content
                    .iter()
                    .filter(|block| {
                        block.get("type").and_then(|v| v.as_str()) == Some("text")
                    })
                    .cloned()
                    .collect();
                if text_only.len() != content.len() {
                    // Reconstruct: join text blocks into a single string
                    let combined: String = text_only
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join("");
                    assistant["content"] = json!(if combined.is_empty() { "Continue".to_string() } else { combined });
                    modified = true;
                }
            }
            // Also clear tool_uses array if present
            if assistant.get("tool_uses").is_some() {
                assistant.as_object_mut().unwrap().remove("tool_uses");
                modified = true;
            }
        }

        // Strip toolResults from user messages
        if let Some(user) = msg.get_mut("userInputMessage") {
            if let Some(context) = user.get_mut("userInputMessageContext") {
                if context.get("toolResults").is_some() {
                    context.as_object_mut().unwrap().remove("toolResults");
                    modified = true;
                }
            }
            // Ensure user message still has content
            let content = user.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                user["content"] = json!("Continue");
                modified = true;
            }
        }
    }

    // Re-run standard sanitizer after stripping
    let sanitizer = ConversationSanitizer {
        enforce_boundary_guards: true,
        insert_sentinels: true,
    };
    let result = sanitizer.sanitize(messages);
    if result.inserted > 0 || result.orphans_repaired > 0 {
        modified = true;
    }

    debug!(modified, "aggressive_sanitize 完成");
    modified
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

    #[test]
    fn deep_sanitize_fills_empty_content() {
        let mut msgs = vec![
            user("hi"),
            json!({"assistantResponseMessage": {"content": ""}}),
            user("bye"),
        ];
        let modified = deep_sanitize(&mut msgs);
        assert!(modified);
        let content = msgs[1]["assistantResponseMessage"]["content"].as_str().unwrap();
        assert_eq!(content, "Continue");
    }

    #[test]
    fn deep_sanitize_deduplicates_tool_results() {
        let mut msgs = vec![
            user("hi"),
            assistant("ok"),
            json!({
                "userInputMessage": {
                    "content": "",
                    "userInputMessageContext": {
                        "toolResults": [
                            {"toolUseId": "id1", "status": "ok", "content": "first"},
                            {"toolUseId": "id1", "status": "error", "content": "duplicate"}
                        ]
                    }
                }
            }),
        ];
        let modified = deep_sanitize(&mut msgs);
        assert!(modified);
        // Should keep the last occurrence of id1
        let results = msgs[2]["userInputMessage"]["userInputMessageContext"]["toolResults"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["content"].as_str().unwrap(), "duplicate");
    }

    #[test]
    fn aggressive_sanitize_strips_tool_data() {
        let mut msgs = vec![
            user("hi"),
            json!({
                "assistantResponseMessage": {
                    "content": [
                        {"type": "text", "text": "Let me search"},
                        {"type": "tool_use", "id": "tu1", "name": "search", "input": {}}
                    ],
                    "tool_uses": [{"id": "tu1", "name": "search"}]
                }
            }),
            json!({
                "userInputMessage": {
                    "content": "",
                    "userInputMessageContext": {
                        "toolResults": [{"toolUseId": "tu1", "content": "result"}]
                    }
                }
            }),
            user("continue"),
        ];
        let modified = aggressive_sanitize(&mut msgs);
        assert!(modified);
        // Assistant content should be text-only
        let content = msgs[1]["assistantResponseMessage"]["content"].as_str().unwrap();
        assert_eq!(content, "Let me search");
        // tool_uses should be removed
        assert!(msgs[1]["assistantResponseMessage"].get("tool_uses").is_none());
        // User tool results should be stripped
        let context = msgs[2]["userInputMessage"].get("userInputMessageContext");
        assert!(context.is_none() || context.unwrap().get("toolResults").is_none());
    }
}
