//! Anthropic Messages API → Kiro conversationState payload conversion.
//!
//! Converts Anthropic-format requests into Kiro's native format for the
//! `generateAssistantResponse` API endpoint.

use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::{debug, warn};

use super::model_map::normalize_model_id;
use crate::config::{ModelRoute, ProviderConfig};
use crate::error::{AppError, Result};

// ---- Constants ----

/// Maximum tool name length in Kiro API (characters, not bytes)
const TOOL_NAME_MAX_LEN: usize = 64;

/// Maximum tool description length
const TOOL_DESC_MAX_LEN: usize = 10_000;

/// Maximum payload size for Kiro API (600KB)
const KIRO_MAX_PAYLOAD_BYTES: usize = 600_000;

/// Chunked policy injected into system prompt
const SYSTEM_CHUNKED_POLICY: &str = "When the Write or Edit tool has content size limits, always comply silently. Never suggest bypassing these limits via alternative tools. Never ask the user whether to switch approaches. Complete all chunked operations without commentary.";

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
        super::model_map::resolve_alias(&resolved_model, aliases)
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

    // 6b. Convert Anthropic cache_control to Kiro cachePoint
    let mut body_for_cache = body.clone();
    super::prompt_cache::convert_cache_control(&mut body_for_cache);

    // 7. Process messages
    let messages = body_for_cache.get("messages").or_else(|| body.get("messages")).and_then(|v| v.as_array());
    let has_tools = kiro_tools.as_array().map(|a| !a.is_empty()).unwrap_or(false);
    let (mut history, current_content, current_images, current_tool_results) =
        process_messages(messages, &full_system_text, thinking_ref, &kiro_model, has_tools);

    // 7b. Apply History Manager (auto-truncate long histories)
    let history_mgr = super::history::HistoryManager::new(super::history::HistoryConfig::default());
    history_mgr.process_history(&mut history);

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
        // Try trimming oldest history pairs
        if let Some(history) = payload["conversationState"].get_mut("history").and_then(|v| v.as_array_mut()) {
            let original_len = history.len();
            while serialized_len > KIRO_MAX_PAYLOAD_BYTES && history.len() > 2 {
                history.remove(0);
                if history.len() > 1 {
                    history.remove(0);
                }
                // Re-serialize to check size (serialize just the history to avoid borrow conflict)
                serialized_len = serde_json::to_vec(history)
                    .map(|v| v.len())
                    .unwrap_or(0)
                    + 200; // overhead for conversationState wrapper
            }
            if serialized_len > KIRO_MAX_PAYLOAD_BYTES {
                return Err(AppError::Request(format!(
                    "Kiro API payload 过大: {} 字节 (最大 {} 字节)，修剪 {} 条历史后仍超限",
                    serialized_len, KIRO_MAX_PAYLOAD_BYTES, original_len - history.len()
                )));
            }
            debug!("Payload 自动修剪: 移除 {} 条历史", original_len - history.len());
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

// ---- Message processing ----

/// Process messages into (history, current_content, current_images, current_tool_results).
fn process_messages(
    messages: Option<&Vec<Value>>,
    system_text: &str,
    thinking_config: Option<&Value>,
    model: &str,
    has_tools: bool,
) -> (Vec<Value>, String, Vec<Value>, Vec<Value>) {
    let messages = match messages {
        Some(m) => m,
        None => {
            // No messages - build minimal history with system prompt
            let history = build_system_history(system_text, thinking_config, model);
            return (history, String::new(), vec![], vec![]);
        }
    };

    if messages.is_empty() {
        let history = build_system_history(system_text, thinking_config, model);
        return (history, String::new(), vec![], vec![]);
    }

    // Prefill discard: if last message is not user, find last user message
    let last_user_idx = messages
        .iter()
        .rposition(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"));

    let effective_messages = match last_user_idx {
        Some(idx) => &messages[..=idx],
        None => messages,
    };

    if effective_messages.is_empty() {
        let history = build_system_history(system_text, thinking_config, model);
        return (history, String::new(), vec![], vec![]);
    }

    // Strip tool content when no tools defined, and repair orphan tool_results
    // Note: the last message (current) is handled separately by extract_current_message,
    // so we only strip/repair the history messages.
    let stripped_messages: Vec<Value> = if !has_tools {
        effective_messages.iter().map(|m| strip_tool_content(m)).collect()
    } else {
        // Only repair orphan tool_results in history messages (not the current message)
        let mut msgs: Vec<Value> = effective_messages[..effective_messages.len().saturating_sub(1)]
            .iter()
            .map(|m| repair_orphan_tool_results(m))
            .collect();
        if let Some(last) = effective_messages.last() {
            msgs.push(last.clone());
        }
        msgs
    };

    // Split: last message = current, rest = history
    let (history_msgs, current_msg) = stripped_messages.split_at(stripped_messages.len() - 1);

    // Build history with system prompt as first entry
    let mut history = build_system_history(system_text, thinking_config, model);

    // Convert history messages
    let converted_history = convert_history_messages(history_msgs);
    history.extend(converted_history);

    // Process current message
    let current = &current_msg[0];
    let (content, images, tool_results) = extract_current_message(current);

    (history, content, images, tool_results)
}

/// Build history entries for system prompt injection.
fn build_system_history(
    system_text: &str,
    thinking_config: Option<&Value>,
    model: &str,
) -> Vec<Value> {
    let mut parts = Vec::new();

    // Thinking prefix
    if let Some(thinking) = thinking_config {
        let thinking_type = thinking.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match thinking_type {
            "enabled" => {
                let budget = thinking
                    .get("budget_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20000);
                parts.push(format!(
                    "<thinking_mode>enabled</thinking_mode><max_thinking_length>{}</max_thinking_length>",
                    budget
                ));
            }
            "adaptive" => {
                parts.push(
                    "<thinking_mode>adaptive</thinking_mode><thinking_effort>high</thinking_effort>"
                        .to_string(),
                );
            }
            _ => {}
        }
    }

    if !system_text.is_empty() {
        parts.push(system_text.to_string());
        parts.push(SYSTEM_CHUNKED_POLICY.to_string());
        parts.push(super::truncation::get_truncation_recovery_system_prompt().to_string());
    }

    if parts.is_empty() {
        return vec![];
    }

    let content = parts.join("\n");

    vec![
        json!({"userInputMessage": {"content": content, "modelId": model, "origin": "AI_EDITOR"}}),
        json!({"assistantResponseMessage": {"content": "I will follow these instructions."}}),
    ]
}

/// Strip tool_calls and tool_results from a message, converting them to text.
/// Used when no tools are defined.
fn strip_tool_content(msg: &Value) -> Value {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let mut result = msg.clone();

    if role == "assistant" {
        // Convert tool_use blocks to text descriptions
        if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
            let mut text_parts = Vec::new();
            let mut has_tool_use = false;
            for block in content {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => {
                        has_tool_use = true;
                        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let input = block.get("input").cloned().unwrap_or(json!({}));
                        text_parts.push(format!("[Called {} with args: {}]", name, input));
                    }
                    _ => {}
                }
            }
            if has_tool_use {
                result["content"] = json!(text_parts.join("\n"));
            }
        }
        // Remove tool_calls field (OpenAI format)
        if result.get("tool_calls").is_some() {
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                let mut text = result.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                for tc in tool_calls {
                    let func = tc.get("function");
                    let name = func.and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                    let args = func.and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("{}");
                    text.push_str(&format!("\n[Called {} with args: {}]", name, args));
                }
                result["content"] = json!(text);
            }
            result.as_object_mut().unwrap().remove("tool_calls");
        }
    }

    if role == "user" {
        // Convert tool_result blocks to text
        if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
            let mut text_parts = Vec::new();
            let mut has_tool_result = false;
            for block in content {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(t.to_string());
                        }
                    }
                    Some("tool_result") => {
                        has_tool_result = true;
                        let tool_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                        let result_text = match block.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(Value::Array(arr)) => {
                                arr.iter()
                                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            }
                            _ => String::new(),
                        };
                        text_parts.push(format!("[Tool result for {}: {}]", tool_id, result_text));
                    }
                    _ => {}
                }
            }
            if has_tool_result {
                result["content"] = json!(text_parts.join("\n"));
            }
        }
    }

    result
}

/// Repair orphan tool_results that have no preceding assistant tool_use.
/// Converts them to text descriptions instead.
fn repair_orphan_tool_results(msg: &Value) -> Value {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    if role != "user" {
        return msg.clone();
    }

    if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
        let has_orphan = content.iter().any(|b| {
            b.get("type").and_then(|v| v.as_str()) == Some("tool_result")
        });
        if !has_orphan {
            return msg.clone();
        }

        let mut result = msg.clone();
        let text_parts: Vec<String> = content
            .iter()
            .map(|b| {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("text") => b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    Some("tool_result") => {
                        let tool_id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                        let result_text = match b.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(Value::Array(arr)) => {
                                arr.iter()
                                    .filter_map(|x| x.get("text").and_then(|v| v.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            }
                            _ => String::new(),
                        };
                        format!("[Tool result for {}: {}]", tool_id, result_text)
                    }
                    _ => String::new(),
                }
            })
            .collect();
        result["content"] = json!(text_parts.join("\n"));
        return result;
    }

    msg.clone()
}

/// Normalize unknown roles to "user" (e.g., "developer", "system" in message array).
fn normalize_role(role: &str) -> &str {
    match role {
        "user" | "assistant" => role,
        _ => "user",
    }
}

/// Convert history messages, merging consecutive same-role messages.
fn convert_history_messages(messages: &[Value]) -> Vec<Value> {
    let mut result = Vec::new();
    let mut buffered_user_content: Vec<String> = Vec::new();
    let mut buffered_user_images: Vec<Value> = Vec::new();
    let mut buffered_user_tool_results: Vec<Value> = Vec::new();
    let mut buffered_assistant_content: Vec<String> = Vec::new();
    let mut buffered_assistant_tool_uses: Vec<Value> = Vec::new();
    let mut current_role: Option<&str> = None;

    let flush_user = |result: &mut Vec<Value>,
                      content_parts: &mut Vec<String>,
                      images: &mut Vec<Value>,
                      tool_results: &mut Vec<Value>| {
        let text = content_parts.join("\n");
        let mut msg = json!({"userInputMessage": {"content": text, "modelId": "auto", "origin": "AI_EDITOR"}});
        if !images.is_empty() {
            msg["userInputMessage"]["images"] = json!(images);
        }
        if !tool_results.is_empty() {
            msg["userInputMessage"]["userInputMessageContext"] =
                json!({"toolResults": json!(tool_results)});
        }
        result.push(msg);
        content_parts.clear();
        images.clear();
        tool_results.clear();
    };

    let flush_assistant = |result: &mut Vec<Value>,
                           content_parts: &mut Vec<String>,
                           tool_uses: &mut Vec<Value>| {
        let text = if content_parts.is_empty() && !tool_uses.is_empty() {
            " ".to_string()
        } else {
            content_parts.join("\n\n")
        };
        let mut msg = json!({"assistantResponseMessage": {"content": text}});
        if !tool_uses.is_empty() {
            msg["assistantResponseMessage"]["toolUses"] = json!(tool_uses);
        }
        result.push(msg);
        content_parts.clear();
        tool_uses.clear();
    };

    for msg in messages {
        let raw_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let role = normalize_role(raw_role);

        if role == "user" {
            if current_role == Some("assistant") {
                flush_assistant(&mut result, &mut buffered_assistant_content, &mut buffered_assistant_tool_uses);
            }
            current_role = Some("user");

            // Extract content, images, tool_results from user message
            if let Some(content) = msg.get("content") {
                if let Some(text) = content.as_str() {
                    buffered_user_content.push(text.to_string());
                } else if let Some(arr) = content.as_array() {
                    for block in arr {
                        let block_type = block.get("type").and_then(|v| v.as_str());
                        match block_type {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    buffered_user_content.push(text.to_string());
                                }
                            }
                            Some("image") => {
                                if let Some(img) = convert_image_block(block) {
                                    buffered_user_images.push(img);
                                }
                            }
                            Some("tool_result") => {
                                if let Some(tr) = convert_tool_result(block) {
                                    buffered_user_tool_results.push(tr);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        } else if role == "assistant" {
            if current_role == Some("user") {
                flush_user(
                    &mut result,
                    &mut buffered_user_content,
                    &mut buffered_user_images,
                    &mut buffered_user_tool_results,
                );
            }
            current_role = Some("assistant");

            // Extract content blocks
            if let Some(content) = msg.get("content") {
                if let Some(text) = content.as_str() {
                    buffered_assistant_content.push(text.to_string());
                } else if let Some(arr) = content.as_array() {
                    let mut thinking_parts = Vec::new();
                    let mut text_parts = Vec::new();

                    for block in arr {
                        let block_type = block.get("type").and_then(|v| v.as_str());
                        match block_type {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    text_parts.push(text.to_string());
                                }
                            }
                            Some("thinking") => {
                                if let Some(thinking) =
                                    block.get("thinking").and_then(|v| v.as_str())
                                {
                                    thinking_parts.push(thinking.to_string());
                                }
                            }
                            Some("tool_use") => {
                                let id = block
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let input =
                                    block.get("input").cloned().unwrap_or(json!({}));
                                buffered_assistant_tool_uses.push(json!({
                                    "toolUseId": id,
                                    "name": name,
                                    "input": input
                                }));
                            }
                            _ => {}
                        }
                    }

                    // Compose content: thinking wrapped in tags + text
                    let thinking_text = thinking_parts.join("");
                    let visible_text = text_parts.join("");

                    if !thinking_text.is_empty() && !visible_text.is_empty() {
                        buffered_assistant_content.push(format!(
                            "<thinking>{}</thinking>\n\n{}",
                            thinking_text, visible_text
                        ));
                    } else if !thinking_text.is_empty() {
                        buffered_assistant_content
                            .push(format!("<thinking>{}</thinking>", thinking_text));
                    } else if !visible_text.is_empty() {
                        buffered_assistant_content.push(visible_text);
                    }
                }
            }
        }
        // Skip other roles (system is handled separately)
    }

    // Flush remaining
    match current_role {
        Some("user") => flush_user(
            &mut result,
            &mut buffered_user_content,
            &mut buffered_user_images,
            &mut buffered_user_tool_results,
        ),
        Some("assistant") => flush_assistant(
            &mut result,
            &mut buffered_assistant_content,
            &mut buffered_assistant_tool_uses,
        ),
        _ => {}
    }

    // Auto-pair trailing orphan user messages
    if let Some(last) = result.last() {
        if last.get("userInputMessage").is_some()
            && !result
                .iter()
                .any(|m| m.get("assistantResponseMessage").is_some())
        {
            // Only user messages in history - add assistant "OK"
            result.push(json!({"assistantResponseMessage": {"content": "OK"}}));
        }
    }

    // Ensure first message is a user message
    if let Some(first) = result.first() {
        if first.get("assistantResponseMessage").is_some() {
            result.insert(
                0,
                json!({"userInputMessage": {"content": "(continued)", "modelId": "auto", "origin": "AI_EDITOR"}}),
            );
        }
    }

    // Ensure alternating roles: insert synthetic assistant between consecutive user messages
    let mut i = 0;
    while i + 1 < result.len() {
        let curr_is_user = result[i].get("userInputMessage").is_some();
        let next_is_user = result[i + 1].get("userInputMessage").is_some();
        if curr_is_user && next_is_user {
            result.insert(
                i + 1,
                json!({"assistantResponseMessage": {"content": "OK"}}),
            );
        }
        i += 1;
    }

    result
}

/// Extract content, images, and tool_results from the current (last) user message.
fn extract_current_message(msg: &Value) -> (String, Vec<Value>, Vec<Value>) {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    if let Some(content) = msg.get("content") {
        if let Some(text) = content.as_str() {
            text_parts.push(text.to_string());
        } else if let Some(arr) = content.as_array() {
            for block in arr {
                let block_type = block.get("type").and_then(|v| v.as_str());
                match block_type {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(text.to_string());
                        }
                    }
                    Some("image") => {
                        if let Some(img) = convert_image_block(block) {
                            images.push(img);
                        }
                    }
                    Some("tool_result") => {
                        if let Some(tr) = convert_tool_result(block) {
                            tool_results.push(tr);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    (text_parts.join("\n"), images, tool_results)
}

// ---- Image conversion ----

fn convert_image_block(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    let source_type = source.get("type").and_then(|v| v.as_str()).unwrap_or("base64");

    let (format, data) = if source_type == "url" {
        // URL source - not directly supported by Kiro, skip
        warn!("Kiro 不支持 URL 类型图片，跳过");
        return None;
    } else {
        // Base64 source
        let media_type = source
            .get("media_type")
            .and_then(|v| v.as_str())
            .unwrap_or("image/png");
        let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
        let format = match media_type {
            "image/jpeg" => "jpeg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => {
                warn!(media_type, "不支持的图片格式，跳过");
                return None;
            }
        };
        (format, data)
    };

    Some(json!({
        "format": format,
        "source": {"bytes": data}
    }))
}

// ---- Tool result conversion ----

fn convert_tool_result(block: &Value) -> Option<Value> {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let content = match block.get("content") {
        Some(Value::String(s)) => vec![json!({"text": s})],
        Some(Value::Array(arr)) => {
            arr.iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                        b.get("text").map(|t| json!({"text": t}))
                    } else {
                        b.as_str().map(|s| json!({"text": s}))
                    }
                })
                .collect()
        }
        _ => vec![json!({"text": ""})],
    };

    let is_error = block
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut result = json!({
        "toolUseId": tool_use_id,
        "content": content,
    });

    if is_error {
        result["status"] = json!("error");
        result["isError"] = json!(true);
    } else {
        result["status"] = json!("success");
    }

    Some(result)
}

// ---- Tool conversion ----

/// Convert Anthropic tools to Kiro tool format.
/// Returns `(kiro_tools, tool_documentation)` where `tool_documentation` contains
/// full descriptions of tools that exceeded the length limit.
fn convert_tools(tools: &[Value], name_map: &mut HashMap<String, String>) -> (Value, String) {
    let mut tool_docs = String::new();
    let kiro_tools: Vec<Value> = tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(|v| v.as_str())?;
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let input_schema = tool
                .get("input_schema")
                .cloned()
                .unwrap_or(json!({}));

            // Shorten tool name if needed
            let (effective_name, was_shortened) = shorten_tool_name(name);
            if was_shortened {
                name_map.insert(effective_name.clone(), name.to_string());
            }

            // Normalize JSON Schema
            let schema = normalize_json_schema(input_schema);

            // Long descriptions: move to system prompt instead of truncating
            let effective_desc = if description.len() > TOOL_DESC_MAX_LEN {
                tool_docs.push_str(&format!("## Tool: {}\n\n{}\n\n", name, description));
                format!("[Full documentation in system prompt under '## Tool: {}']", name)
            } else {
                description.to_string()
            };

            Some(json!({
                "toolSpecification": {
                    "name": effective_name,
                    "description": effective_desc,
                    "inputSchema": {"json": schema}
                }
            }))
        })
        .collect();

    (json!(kiro_tools), tool_docs)
}

/// Shorten a tool name if it exceeds the Kiro limit.
/// Returns (shortened_name, was_shortened).
fn shorten_tool_name(name: &str) -> (String, bool) {
    if name.chars().count() <= TOOL_NAME_MAX_LEN {
        return (name.to_string(), false);
    }

    // Hash-based shortening: prefix + "_" + 8 hex chars
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let hash = hasher.finish();
    let hash_hex = format!("{:08x}", hash as u32);

    let prefix_len = TOOL_NAME_MAX_LEN - 1 - 8; // 55 chars
    let prefix: String = name.chars().take(prefix_len).collect();

    (format!("{}_{}", prefix, hash_hex), true)
}

/// Normalize a JSON Schema for Kiro compatibility.
/// Kiro API rejects `additionalProperties` and empty `required` arrays.
/// Recursively processes nested schemas.
fn normalize_json_schema(mut schema: Value) -> Value {
    if !schema.is_object() {
        return json!({"type": "object", "properties": {}});
    }

    // First, recursively normalize nested schemas in properties
    if let Some(props) = schema.get_mut("properties") {
        if let Some(obj) = props.as_object_mut() {
            for (_key, val) in obj.iter_mut() {
                if val.is_object() {
                    *val = normalize_json_schema(val.take());
                }
            }
        }
    }
    // Recursively normalize items schema (for arrays)
    if let Some(items) = schema.get_mut("items") {
        if items.is_object() {
            let taken = items.take();
            *items = normalize_json_schema(taken);
        }
    }

    let obj = schema.as_object_mut().unwrap();

    // Ensure type is "object"
    if obj
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        obj.insert("type".to_string(), json!("object"));
    }

    // Ensure properties exists
    if !obj
        .get("properties")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        obj.insert("properties".to_string(), json!({}));
    }

    // Handle required: keep non-empty, remove empty
    match obj.get("required") {
        None => {
            // No required field — don't add empty one
        }
        Some(Value::Array(arr)) => {
            let filtered: Vec<Value> = arr
                .iter()
                .filter(|v| v.is_string())
                .cloned()
                .collect();
            if filtered.is_empty() {
                obj.remove("required");
            } else {
                obj.insert("required".to_string(), json!(filtered));
            }
        }
        _ => {
            obj.remove("required");
        }
    }

    // Remove additionalProperties — Kiro API rejects it
    obj.remove("additionalProperties");

    schema
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
    use crate::config::{ProviderFormat, ProviderQuirks, ServerConfig};

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
        assert_eq!(history.len(), 2); // user (system) + assistant ("I will follow...")
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
        assert_eq!(tool_name.chars().count(), 64);
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
