//! OpenAI Responses API → Kiro conversion.
//!
//! Handles the `/v1/responses` endpoint used by Codex CLI.
//! Converts OpenAI Responses API format to Kiro conversationState.

use serde_json::{json, Value};
use std::collections::HashMap;

use super::request::anthropic_to_kiro;
use crate::config::{ModelRoute, ProviderConfig};
use crate::error::Result;

/// Convert OpenAI Responses API request to Kiro payload.
///
/// The Responses API has a different message structure than Chat Completions:
/// - `input`: array of items (message, function_call, function_call_output, etc.)
/// - `instructions`: system prompt
/// - `tools`: tool definitions
/// - `model`: model name
///
/// Strategy: convert to Anthropic Messages format first, then use anthropic_to_kiro.
pub fn responses_to_kiro(
    body: &Value,
    provider: &ProviderConfig,
    global_routes: &[ModelRoute],
) -> Result<(Value, HashMap<String, String>)> {
    // Convert Responses API format to Anthropic Messages format
    let anthropic_body = responses_to_anthropic(body);
    anthropic_to_kiro(&anthropic_body, provider, global_routes)
}

/// Convert OpenAI Responses API format to Anthropic Messages format.
fn responses_to_anthropic(body: &Value) -> Value {
    let mut messages = Vec::new();
    let mut system_text = String::new();

    // Extract instructions as system prompt
    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        system_text = instructions.to_string();
    }

    // Extract model
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("claude-sonnet-4.5");

    // Process input items
    if let Some(input) = body.get("input").and_then(|v| v.as_array()) {
        let mut pending_tool_calls: HashMap<String, (String, String)> = HashMap::new(); // call_id -> (name, arguments)

        for item in input {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match item_type {
                "message" => {
                    let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                    let content = extract_message_content(item);

                    match role {
                        "user" => {
                            messages.push(json!({"role": "user", "content": content}));
                        }
                        "assistant" => {
                            // Check for tool calls in the message
                            let mut content_blocks = Vec::new();

                            if !content.is_empty() {
                                content_blocks.push(json!({"type": "text", "text": content}));
                            }

                            // Check for function_call outputs in the content
                            if let Some(output) = item.get("output") {
                                if let Some(arr) = output.as_array() {
                                    for part in arr {
                                        if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                                content_blocks.push(json!({"type": "text", "text": text}));
                                            }
                                        }
                                    }
                                }
                            }

                            if content_blocks.is_empty() {
                                content_blocks.push(json!({"type": "text", "text": "OK"}));
                            }

                            messages.push(json!({"role": "assistant", "content": content_blocks}));
                        }
                        _ => {}
                    }
                }
                "function_call" => {
                    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let arguments = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                    pending_tool_calls.insert(call_id.to_string(), (name.to_string(), arguments.to_string()));
                }
                "function_call_output" => {
                    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let output = item.get("output").and_then(|v| v.as_str()).unwrap_or("");

                    // Create assistant message with tool_use if we have the pending call
                    if let Some((name, args)) = pending_tool_calls.remove(call_id) {
                        let input: Value = serde_json::from_str(&args).unwrap_or(json!({}));
                        messages.push(json!({
                            "role": "assistant",
                            "content": [
                                {"type": "tool_use", "id": call_id, "name": name, "input": input}
                            ]
                        }));
                    }

                    // Create user message with tool_result
                    messages.push(json!({
                        "role": "user",
                        "content": [
                            {"type": "tool_result", "tool_use_id": call_id, "content": output}
                        ]
                    }));
                }
                "custom_tool_call" => {
                    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let input = item.get("input").cloned().unwrap_or(json!({}));
                    pending_tool_calls.insert(call_id.to_string(), (name.to_string(), input.to_string()));
                }
                "custom_tool_call_output" => {
                    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let output = item.get("output").and_then(|v| v.as_str()).unwrap_or("");

                    if let Some((name, args)) = pending_tool_calls.remove(call_id) {
                        let input: Value = serde_json::from_str(&args).unwrap_or(json!({}));
                        messages.push(json!({
                            "role": "assistant",
                            "content": [
                                {"type": "tool_use", "id": call_id, "name": name, "input": input}
                            ]
                        }));
                    }

                    messages.push(json!({
                        "role": "user",
                        "content": [
                            {"type": "tool_result", "tool_use_id": call_id, "content": output}
                        ]
                    }));
                }
                "web_search_call" | "image_generation_call" => {
                    // Summarize as text
                    let summary = format!("[{} completed]", item_type);
                    messages.push(json!({"role": "assistant", "content": summary}));
                }
                _ => {
                    // Unknown item type - try to extract as text
                    if let Some(text) = item.get("text").or_else(|| item.get("content")).and_then(|v| v.as_str()) {
                        messages.push(json!({"role": "user", "content": text}));
                    }
                }
            }
        }
    }

    // Convert tools
    let tools: Vec<Value> = body.get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().map(|tool| {
                let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let description = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
                let parameters = tool.get("parameters").cloned().unwrap_or(json!({}));
                json!({
                    "name": name,
                    "description": description,
                    "input_schema": parameters
                })
            }).collect()
        })
        .unwrap_or_default();

    let mut result = json!({
        "model": model,
        "messages": messages,
    });

    if !system_text.is_empty() {
        result["system"] = json!(system_text);
    }
    if !tools.is_empty() {
        result["tools"] = json!(tools);
    }

    // Forward max_tokens
    if let Some(max_output) = body.get("max_output_tokens").or_else(|| body.get("max_tokens")) {
        result["max_tokens"] = max_output.clone();
    }

    result
}

/// Extract text content from a Responses API message item.
fn extract_message_content(item: &Value) -> String {
    // Try content field (string or array)
    if let Some(content) = item.get("content") {
        if let Some(s) = content.as_str() {
            return s.to_string();
        }
        if let Some(arr) = content.as_array() {
            let texts: Vec<&str> = arr
                .iter()
                .filter_map(|part| {
                    if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                        part.get("text").and_then(|v| v.as_str())
                    } else if part.get("type").and_then(|v| v.as_str()) == Some("input_text") {
                        part.get("text").and_then(|v| v.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            return texts.join("\n");
        }
    }

    // Try output field (for assistant messages)
    if let Some(output) = item.get("output") {
        if let Some(arr) = output.as_array() {
            let texts: Vec<&str> = arr
                .iter()
                .filter_map(|part| {
                    if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                        part.get("text").and_then(|v| v.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            return texts.join("\n");
        }
    }

    String::new()
}

/// Convert Kiro EventStream events to OpenAI Responses API SSE format.
pub fn events_to_responses_sse(
    events: Vec<super::eventstream::Event>,
    response_id: &str,
) -> Vec<String> {
    let mut sse_events = Vec::new();

    // response.created
    sse_events.push(format!(
        "event: response.created\ndata: {}\n\n",
        json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress"
            }
        })
    ));

    // response.output_item.added
    sse_events.push(format!(
        "event: response.output_item.added\ndata: {}\n\n",
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "message",
                "id": format!("msg_{}", response_id),
                "role": "assistant",
                "status": "in_progress"
            }
        })
    ));

    let mut full_text = String::new();
    let mut tool_uses = Vec::new();

    for event in &events {
        match event {
            super::eventstream::Event::AssistantResponse { content }
                if !content.is_empty() => {
                    full_text.push_str(content);
                    sse_events.push(format!(
                        "event: response.output_text.delta\ndata: {}\n\n",
                        json!({
                            "type": "response.output_text.delta",
                            "output_index": 0,
                            "content_index": 0,
                            "delta": content
                        })
                    ));
                }
            super::eventstream::Event::ToolUse { tool_use_id, name, input, stop } => {
                tool_uses.push((tool_use_id.clone(), name.clone(), input.clone()));
                if *stop {
                    let input_json: Value = serde_json::from_str(input).unwrap_or(json!({}));
                    sse_events.push(format!(
                        "event: response.function_call\ndata: {}\n\n",
                        json!({
                            "type": "response.function_call",
                            "call_id": tool_use_id,
                            "name": name,
                            "arguments": input_json
                        })
                    ));
                }
            }
            _ => {}
        }
    }

    // response.output_text.done
    sse_events.push(format!(
        "event: response.output_text.done\ndata: {}\n\n",
        json!({
            "type": "response.output_text.done",
            "output_index": 0,
            "content_index": 0,
            "text": full_text
        })
    ));

    // response.output_item.done
    sse_events.push(format!(
        "event: response.output_item.done\ndata: {}\n\n",
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "id": format!("msg_{}", response_id),
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": full_text}]
            }
        })
    ));

    // response.completed
    sse_events.push(format!(
        "event: response.completed\ndata: {}\n\n",
        json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": full_text}]
                }]
            }
        })
    ));

    sse_events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_to_anthropic_basic() {
        let body = json!({
            "model": "claude-sonnet-4.5",
            "instructions": "You are helpful.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}
            ]
        });

        let result = responses_to_anthropic(&body);
        assert_eq!(result["model"], "claude-sonnet-4.5");
        assert_eq!(result["system"], "You are helpful.");
        assert_eq!(result["messages"][0]["role"], "user");
    }

    #[test]
    fn responses_to_anthropic_tool_calls() {
        let body = json!({
            "model": "test",
            "input": [
                {"type": "message", "role": "user", "content": "Search for X"},
                {"type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{\"q\":\"X\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "Result: X found"}
            ]
        });

        let result = responses_to_anthropic(&body);
        let msgs = result["messages"].as_array().unwrap();
        // Should have: user, assistant(tool_use), user(tool_result)
        assert!(msgs.len() >= 3);
    }

    #[test]
    fn responses_to_anthropic_with_tools() {
        let body = json!({
            "model": "test",
            "input": [],
            "tools": [
                {"name": "search", "description": "Search", "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}
            ]
        });

        let result = responses_to_anthropic(&body);
        assert!(result["tools"].as_array().unwrap().len() == 1);
        assert_eq!(result["tools"][0]["name"], "search");
    }
}
