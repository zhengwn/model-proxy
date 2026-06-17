//! Anthropic Messages API → Kiro conversationState payload conversion.
//!
//! Converts Anthropic-format requests into Kiro's native format for the
//! `generateAssistantResponse` API endpoint.
//!
//! Split into focused submodules:
//! - [`messages`] — message-array processing, history construction
//! - [`content`] — image and tool-result block conversion
//! - [`tools`] — tool spec conversion, name shortening, JSON Schema normalization

use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::debug;

use super::model_map::normalize_model_id;
use crate::config::{ModelRoute, ProviderConfig};
use crate::error::{AppError, Result};

mod content;
mod messages;
mod tools;

use messages::process_messages;
use tools::convert_tools;

// ---- Constants ----

/// Maximum payload size for Kiro API (600KB)
const KIRO_MAX_PAYLOAD_BYTES: usize = 600_000;

// ---- Public API ----

/// Convert an Anthropic Messages API request to Kiro conversationState payload.
///
/// Returns `(kiro_payload, tool_name_map)` where `tool_name_map` maps
/// shortened tool names back to original names.
pub fn anthropic_to_kiro(
    body: &Value,
    provider: &ProviderConfig,
    global_routes: &[ModelRoute],
) -> Result<(Value, HashMap<String, String>)> {
    // 1. Model ID normalization with alias resolution
    let requested_model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let resolved_model = provider
        .resolve_model_with_routes(Some(requested_model), global_routes);
    let aliased_model = if let Some(aliases) = provider.kiro_config.as_ref().and_then(|k| k.model_aliases.as_ref()) {
        super::model_map::resolve_alias(resolved_model, aliases)
    } else {
        resolved_model.to_string()
    };
    let kiro_model = normalize_model_id(&aliased_model)
        .unwrap_or_else(|| "claude-sonnet-4.5".to_string());

    let mut tool_name_map: HashMap<String, String> = HashMap::new();

    // 2. Extract system prompt
    let system_text = extract_system_text(body);

    // 3. Extract thinking config (supports both Anthropic thinking and OpenAI reasoning_effort)
    let thinking_config = body.get("thinking");
    let reasoning_effort = body.get("reasoning_effort").and_then(|v| v.as_str());
    let effective_thinking = if thinking_config.is_some() {
        thinking_config.cloned()
    } else if let Some(effort) = reasoning_effort {
        // Map OpenAI reasoning_effort to Anthropic thinking config
        let budget_pct = match effort {
            "none" => 0.0,
            "minimal" => 0.10,
            "low" => 0.20,
            "medium" => 0.50,
            "high" => 0.80,
            "xhigh" => 0.95,
            _ => 0.50,
        };
        let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(16000);
        let budget = (max_tokens as f64 * budget_pct) as u64;
        if budget > 0 {
            Some(json!({"type": "enabled", "budget_tokens": budget}))
        } else {
            None
        }
    } else {
        None
    };
    let thinking_ref = effective_thinking.as_ref();

    // 4. Extract max_tokens for reasoning effort budget calculation
    // Note: Kiro API does not natively support temperature/top_p/stop_sequences
    // in conversationState, so we only forward max_tokens for thinking budget.

    // 5. Extract tools and convert
    let tools = body.get("tools").and_then(|v| v.as_array());
    let (mut kiro_tools, tool_docs) = match tools {
        Some(t) => convert_tools(t, &mut tool_name_map),
        None => (json!([]), String::new()),
    };

    // 5b. Inject web_search tool if enabled
    if provider.kiro_config.as_ref().and_then(|k| k.web_search_enabled).unwrap_or(false) {
        if let Some(arr) = kiro_tools.as_array_mut() {
            arr.push(super::mcp::web_search_tool_definition());
        }
    }

    // 6. Merge system text with tool documentation
    let mut full_system_text = system_text;
    if !tool_docs.is_empty() {
        if full_system_text.is_empty() {
            full_system_text = tool_docs;
        } else {
            full_system_text.push_str("\n\n# Tool Documentation\n\n");
            full_system_text.push_str(&tool_docs);
        }
    }

    // 6b. Inject agentic system prompt when tools are detected
    let has_file_tools = tools.map(|t| t.iter().any(|tool| {
        let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
        matches!(name, "Write" | "Edit" | "create_file" | "edit_file" | "NotebookEdit")
    })).unwrap_or(false);

    if has_file_tools && provider.kiro_config.as_ref().and_then(|k| k.agentic_prompt_injection).unwrap_or(false) {
        let agentic_prompt = format!(
            "Current time: {}. File operations (Write/Edit) must not exceed 50KB per call. \
             Always comply silently with content size limits. Never suggest bypassing limits.",
            chrono::Utc::now().to_rfc3339()
        );
        if full_system_text.is_empty() {
            full_system_text = agentic_prompt;
        } else {
            full_system_text.push_str("\n\n");
            full_system_text.push_str(&agentic_prompt);
        }
    }

    // 6b. Convert Anthropic cache_control to Kiro cachePoint
    let mut body_for_cache = body.clone();
    super::prompt_cache::convert_cache_control(&mut body_for_cache);

    // 7. Process messages
    let messages = body_for_cache.get("messages").or_else(|| body.get("messages")).and_then(|v| v.as_array());
    let has_tools = kiro_tools.as_array().map(|a| !a.is_empty()).unwrap_or(false);
    // Build reverse name map: original_name → shortened_name (for history tool_use fixup)
    let reverse_name_map: HashMap<String, String> = tool_name_map
        .iter()
        .map(|(short, orig)| (orig.clone(), short.clone()))
        .collect();
    let (mut history, current_content, current_images, current_tool_results) =
        process_messages(messages, &full_system_text, thinking_ref, &kiro_model, has_tools, &reverse_name_map);

    // 7b. Apply History Manager (auto-truncate long histories)
    let history_mgr = super::history::HistoryManager::new(super::history::HistoryConfig::default());
    history_mgr.process_history(&mut history);

    // 7c. Add placeholder tool definitions for tools referenced in history but missing from current tools
    if let Some(tools_arr) = kiro_tools.as_array() {
        let current_tool_names: std::collections::HashSet<&str> = tools_arr
            .iter()
            .filter_map(|t| t["toolSpecification"]["name"].as_str())
            .collect();
        let mut missing_names = std::collections::HashSet::new();
        for hist_msg in &history {
            if let Some(tool_uses) = hist_msg.pointer("/assistantResponseMessage/toolUses")
                .and_then(|v| v.as_array())
            {
                for tu in tool_uses {
                    if let Some(name) = tu["name"].as_str() {
                        if !current_tool_names.contains(name) {
                            missing_names.insert(name.to_string());
                        }
                    }
                }
            }
        }
        if !missing_names.is_empty() {
            if let Some(arr) = kiro_tools.as_array_mut() {
                for name in &missing_names {
                    arr.push(json!({
                        "toolSpecification": {
                            "name": name,
                            "description": "Previously used tool",
                            "inputSchema": {"json": {"type": "object", "properties": {}}}
                        }
                    }));
                }
                debug!("为历史消息中引用的 {} 个工具添加了 placeholder 定义", missing_names.len());
            }
        }
    }

    // 8. Build currentMessage
    let mut user_input_message = json!({
        "content": current_content,
        "modelId": kiro_model,
        "origin": "AI_EDITOR",
    });

    if !current_images.is_empty() {
        user_input_message["images"] = json!(current_images);
    }

    let mut context = json!({});
    if kiro_tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
        context["tools"] = kiro_tools;
    }
    if !current_tool_results.is_empty() {
        context["toolResults"] = json!(current_tool_results);
    }
    user_input_message["userInputMessageContext"] = context;

    // 9. Build conversationState
    let conversation_id = uuid_v4();
    let agent_continuation_id = uuid_v4();

    let mut conversation_state = json!({
        "conversationId": conversation_id,
        "agentContinuationId": agent_continuation_id,
        "agentTaskType": "vibe",
        "chatTriggerType": "MANUAL",
        "currentMessage": {
            "userInputMessage": user_input_message
        },
    });

    if !history.is_empty() {
        conversation_state["history"] = json!(history);
    }

    let mut payload = json!({"conversationState": conversation_state});

    // 10. Payload size guard with auto-trim
    let mut serialized_len = serde_json::to_vec(&payload)
        .map(|v| v.len())
        .unwrap_or(0);
    if serialized_len > KIRO_MAX_PAYLOAD_BYTES {
        let has_history = payload["conversationState"].get("history")
            .and_then(|v| v.as_array())
            .map(|a| a.len() > 2)
            .unwrap_or(false);

        if has_history {
            let original_len = payload["conversationState"]["history"]
                .as_array().map(|a| a.len()).unwrap_or(0);

            while serialized_len > KIRO_MAX_PAYLOAD_BYTES {
                let len = payload["conversationState"]["history"]
                    .as_array().map(|a| a.len()).unwrap_or(0);
                if len <= 2 {
                    break;
                }
                // Remove oldest pair
                if let Some(history) = payload["conversationState"].get_mut("history")
                    .and_then(|v| v.as_array_mut())
                {
                    history.remove(0);
                    if history.len() > 1 {
                        history.remove(0);
                    }
                }
                // Re-serialize the full payload to get accurate size
                serialized_len = serde_json::to_vec(&payload)
                    .map(|v| v.len())
                    .unwrap_or(0);
            }

            let current_len = payload["conversationState"]["history"]
                .as_array().map(|a| a.len()).unwrap_or(0);

            if serialized_len > KIRO_MAX_PAYLOAD_BYTES {
                return Err(AppError::Request(format!(
                    "Kiro API payload 过大: {} 字节 (最大 {} 字节)，修剪 {} 条历史后仍超限",
                    serialized_len, KIRO_MAX_PAYLOAD_BYTES, original_len - current_len
                )));
            }
            debug!("Payload 自动修剪: 移除 {} 条历史", original_len - current_len);
        } else {
            return Err(AppError::Request(format!(
                "Kiro API payload 过大: {} 字节 (最大 {} 字节)",
                serialized_len, KIRO_MAX_PAYLOAD_BYTES
            )));
        }
    }

    Ok((payload, tool_name_map))
}

// ---- System prompt extraction ----

fn extract_system_text(body: &Value) -> String {
    match body.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let texts: Vec<&str> = arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect();
            texts.join("\n")
        }
        _ => String::new(),
    }
}

// ---- UUID generation ----

fn uuid_v4() -> String {
    // Simple UUID v4 generation without external crate
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

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use super::tools::{normalize_json_schema, shorten_tool_name};
    use crate::config::{ProviderFormat, ProviderQuirks};

    fn test_provider() -> ProviderConfig {
        ProviderConfig {
            name: "test".to_string(),
            base_url: "https://q.us-east-1.amazonaws.com".to_string(),
            api_key: "test-key".to_string(),
            model: "claude-sonnet-4.5".to_string(),
            format: ProviderFormat::Kiro,
            quirks: ProviderQuirks::default(),
            model_routes: vec![],
            kiro_config: None,
        }
    }

    #[test]
    fn basic_user_message() {
        let body = json!({
            "model": "claude-sonnet-4-5-20250929",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let provider = test_provider();
        let (payload, _) = anthropic_to_kiro(&body, &provider, &[]).unwrap();

        let cs = &payload["conversationState"];
        assert_eq!(cs["chatTriggerType"], "MANUAL");
        assert_eq!(cs["agentTaskType"], "vibe");
        assert!(cs["conversationId"].as_str().is_some());

        let current = &cs["currentMessage"]["userInputMessage"];
        assert_eq!(current["content"], "Hello");
        assert_eq!(current["modelId"], "claude-sonnet-4.5");
        assert_eq!(current["origin"], "AI_EDITOR");
    }

    #[test]
    fn system_prompt_injected_as_history() {
        let body = json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 1024,
            "system": "You are a helpful assistant.",
            "messages": [
                {"role": "user", "content": "Hi"}
            ]
        });
        let provider = test_provider();
        let (payload, _) = anthropic_to_kiro(&body, &provider, &[]).unwrap();

        let history = payload["conversationState"]["history"].as_array().unwrap();
        // user (system) + assistant ("I will follow...") + user ("Continue" sentinel from sanitizer)
        assert!(history.len() >= 2);
        assert!(history[0]["userInputMessage"]["content"]
            .as_str()
            .unwrap()
            .contains("You are a helpful assistant."));
        assert_eq!(
            history[1]["assistantResponseMessage"]["content"],
            "I will follow these instructions."
        );
    }

    #[test]
    fn tool_conversion_with_name_shortening() {
        let long_name = "a".repeat(100);
        let body = json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hi"}],
            "tools": [{
                "name": long_name,
                "description": "A tool",
                "input_schema": {"type": "object", "properties": {}}
            }]
        });
        let provider = test_provider();
        let (payload, name_map) = anthropic_to_kiro(&body, &provider, &[]).unwrap();

        let tools = payload["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"]
            .as_array()
            .unwrap();
        assert_eq!(tools.len(), 1);

        let tool_name = tools[0]["toolSpecification"]["name"].as_str().unwrap();
        assert_eq!(tool_name.chars().count(), 63);
        assert!(name_map.contains_key(tool_name));
    }

    #[test]
    fn tool_choice_conversion_tool_result() {
        let body = json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_abc123", "content": "result text"},
                    {"type": "text", "extra": "ignored"}
                ]}
            ],
            "tools": [{"name": "test_tool", "description": "A test tool", "input_schema": {"type": "object"}}]
        });
        let provider = test_provider();
        let (payload, _) = anthropic_to_kiro(&body, &provider, &[]).unwrap();

        let current = &payload["conversationState"]["currentMessage"]["userInputMessage"];
        let tool_results = current["userInputMessageContext"]["toolResults"]
            .as_array()
            .unwrap();
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_results[0]["toolUseId"], "toolu_abc123");
        assert_eq!(tool_results[0]["status"], "success");
    }

    #[test]
    fn thinking_config_injection() {
        let body = json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 1024,
            "thinking": {"type": "enabled", "budget_tokens": 50000},
            "messages": [{"role": "user", "content": "Think hard"}]
        });
        let provider = test_provider();
        let (payload, _) = anthropic_to_kiro(&body, &provider, &[]).unwrap();

        let history = payload["conversationState"]["history"].as_array().unwrap();
        let content = history[0]["userInputMessage"]["content"].as_str().unwrap();
        assert!(content.contains("<thinking_mode>enabled</thinking_mode>"));
        assert!(content.contains("<max_thinking_length>50000</max_thinking_length>"));
    }

    #[test]
    fn normalize_json_schema_defaults() {
        let schema = json!({"type": "object", "properties": {"x": {"type": "string"}}});
        let result = normalize_json_schema(schema);
        assert_eq!(result["type"], "object");
        // Kiro API: empty required and additionalProperties should be removed
        assert!(result.get("required").is_none());
        assert!(result.get("additionalProperties").is_none());
    }

    #[test]
    fn shorten_tool_name_noop() {
        let (name, shortened) = shorten_tool_name("short_name");
        assert_eq!(name, "short_name");
        assert!(!shortened);
    }

    #[test]
    fn uuid_v4_format() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        // Check version nibble
        assert_eq!(id.as_bytes()[14], b'4');
    }
}
