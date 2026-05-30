//! Prompt caching support for Kiro API.
//!
//! Converts Anthropic's `cache_control` markers to Kiro's `cachePoint` format,
//! enabling prompt caching to reduce token costs on repeated requests.

use serde_json::{json, Value};
use tracing::debug;

/// Convert Anthropic cache_control markers to Kiro cachePoint format.
///
/// Anthropic uses `cache_control: {"type": "ephemeral"}` on content blocks.
/// Kiro uses `cachePoint: true` on tool specifications and history entries.
pub fn convert_cache_control(body: &mut Value) {
    let mut has_cache_markers = false;

    // Check system prompt for cache_control
    if let Some(system) = body.get_mut("system") {
        if let Value::Array(blocks) = system {
            for block in blocks.iter_mut() {
                if block.get("cache_control").is_some() {
                    has_cache_markers = true;
                    // Remove cache_control from block (Kiro doesn't use it on system blocks)
                    if let Some(obj) = block.as_object_mut() {
                        obj.remove("cache_control");
                    }
                }
            }
        }
    }

    // Check messages for cache_control
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

    // Check tools for cache_control and convert to cachePoint
    if let Some(tools) = body.get_mut("tools") {
        if let Value::Array(tool_arr) = tools {
            for tool in tool_arr.iter_mut() {
                if tool.get("cache_control").is_some() {
                    has_cache_markers = true;
                    // Mark this tool for caching
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
/// This tells Kiro to cache specific history entries for faster subsequent requests.
pub fn add_history_cache_points(conversation_state: &mut Value, system_history_len: usize) {
    // Mark the system prompt entries as cacheable
    if let Some(history) = conversation_state.get_mut("history").and_then(|v| v.as_array_mut()) {
        for (i, entry) in history.iter_mut().enumerate() {
            if i < system_history_len {
                // System prompt entries are good cache candidates
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("cachePoint".to_string(), json!(true));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(state["history"][3].get("cachePoint").is_none());
    }
}
