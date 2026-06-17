//! Message-array processing: history construction, role normalization,
//! tool-content stripping/repair, and current-message extraction.

use serde_json::{json, Value};
use std::collections::HashMap;

use super::content::{convert_image_block, convert_openai_image_block, convert_tool_result};
use crate::convert::kiro::sanitize::ConversationSanitizer;

/// Chunked policy injected into system prompt.
const SYSTEM_CHUNKED_POLICY: &str = "When the Write or Edit tool has content size limits, always comply silently. Never suggest bypassing these limits via alternative tools. Never ask the user whether to switch approaches. Complete all chunked operations without commentary.";

/// Process messages into (history, current_content, current_images, current_tool_results).
pub(super) fn process_messages(
    messages: Option<&Vec<Value>>,
    system_text: &str,
    thinking_config: Option<&Value>,
    model: &str,
    has_tools: bool,
    reverse_name_map: &HashMap<String, String>,
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
        effective_messages.iter().map(strip_tool_content).collect()
    } else {
        // Only repair orphan tool_results in history messages (not the current message)
        let mut msgs: Vec<Value> = effective_messages[..effective_messages.len().saturating_sub(1)]
            .iter()
            .map(repair_orphan_tool_results)
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
    let converted_history = convert_history_messages(history_msgs, reverse_name_map);
    history.extend(converted_history);

    // Sanitize history: enforce alternation, boundary guards, orphan repair
    let sanitizer = ConversationSanitizer::new();
    let _sanitize_result = sanitizer.sanitize(&mut history);

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
        parts.push(crate::convert::kiro::truncation::get_truncation_recovery_system_prompt().to_string());
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
/// `reverse_name_map` maps original tool names to shortened names (for history tool_use fixup).
fn convert_history_messages(messages: &[Value], reverse_name_map: &HashMap<String, String>) -> Vec<Value> {
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
                            Some("image_url") => {
                                if let Some(img) = convert_openai_image_block(block) {
                                    buffered_user_images.push(img);
                                }
                            }
                            Some("tool_result") => {
                                let (tr, imgs) = convert_tool_result(block);
                                if let Some(tr) = tr {
                                    buffered_user_tool_results.push(tr);
                                }
                                buffered_user_images.extend(imgs);
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
                                // Apply tool name shortening to history entries for consistency
                                let effective_name = reverse_name_map
                                    .get(name)
                                    .map(|s| s.as_str())
                                    .unwrap_or(name);
                                buffered_assistant_tool_uses.push(json!({
                                    "toolUseId": id,
                                    "name": effective_name,
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
                    Some("image_url") => {
                        if let Some(img) = convert_openai_image_block(block) {
                            images.push(img);
                        }
                    }
                    Some("tool_result") => {
                        let (tr, imgs) = convert_tool_result(block);
                        if let Some(tr) = tr {
                            tool_results.push(tr);
                        }
                        images.extend(imgs);
                    }
                    _ => {}
                }
            }
        }
    }

    (text_parts.join("\n"), images, tool_results)
}
