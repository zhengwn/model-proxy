//! Anthropic Messages API → OpenAI Chat Completions request conversion.

use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::{debug, warn};

use super::content::{
    anthropic_id_to_openai, convert_content, convert_tool_choice, convert_tools, sanitized_tool_name,
};
use super::reasoning::{
    is_openai_o_series, json_schema_instruction, resolve_reasoning_effort,
    response_format_unavailable, supports_reasoning_effort,
};
use crate::server::state::strip_leading_anthropic_billing_header;

pub(crate) fn anthropic_to_openai(
    body: Value,
    provider: &crate::config::ProviderConfig,
    global_routes: &[crate::config::ModelRoute],
) -> (Value, HashMap<String, String>) {
    let quirks = &provider.quirks;
    let provider_model = provider
        .resolve_model_with_routes(body.get("model").and_then(|v| v.as_str()), global_routes)
        .to_string();
    let route_reasoning_effort = provider
        .resolve_route_reasoning_effort_with_routes(
            body.get("model").and_then(|v| v.as_str()),
            global_routes,
        )
        .map(str::to_string);
    let mut openai = json!({});

    openai["model"] = json!(provider_model.clone());

    let mut sanitized_to_original = HashMap::new();
    if let Some(tools) = body.get("tools") {
        openai["tools"] = convert_tools(tools, &mut sanitized_to_original);
    }
    let original_to_sanitized: HashMap<String, String> = sanitized_to_original
        .iter()
        .map(|(sanitized, original)| (original.clone(), sanitized.clone()))
        .collect();

    let mut messages = Vec::new();

    if let Some(system) = body.get("system") {
        match system {
            Value::String(text) => {
                let text = strip_leading_anthropic_billing_header(text);
                if !text.is_empty() {
                    messages.push(json!({"role": "system", "content": text}));
                }
            }
            Value::Array(arr) => {
                let mut is_first = true;
                for block in arr {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        let text = if is_first {
                            is_first = false;
                            strip_leading_anthropic_billing_header(text)
                        } else {
                            text
                        };
                        if text.is_empty() {
                            continue;
                        }
                        let mut sys_msg = json!({"role": "system", "content": text});
                        if let Some(cc) = block.get("cache_control") {
                            sys_msg["cache_control"] = cc.clone();
                        }
                        messages.push(sys_msg);
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in arr {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = msg.get("content").cloned().unwrap_or(json!(""));

            if role == "user" {
                if let Some(content_arr) = content.as_array() {
                    let mut tool_results = Vec::new();
                    let mut non_tool_blocks = Vec::new();
                    let mut has_tool_result = false;
                    for block in content_arr {
                        let block_type = block.get("type").and_then(|v| v.as_str());
                        if block_type == Some("tool_result") {
                            has_tool_result = true;
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let openai_id = anthropic_id_to_openai(tool_use_id);
                            let tool_content = block
                                .get("content")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .or_else(|| {
                                    block.get("content").and_then(|v| v.as_array()).map(|arr| {
                                        let texts: Vec<String> = arr
                                            .iter()
                                            .map(|b| {
                                                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                                    t.to_string()
                                                } else {
                                                    // Serialize non-text blocks (images, etc.) as JSON
                                                    serde_json::to_string(b).unwrap_or_default()
                                                }
                                            })
                                            .collect();
                                        texts.join("\n")
                                    })
                                })
                                .unwrap_or_default();
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": openai_id,
                                "content": tool_content
                            }));
                        } else {
                            non_tool_blocks.push(block.clone());
                        }
                    }
                    if has_tool_result {
                        messages.extend(tool_results);
                        if !non_tool_blocks.is_empty() {
                            let converted = convert_content(&Value::Array(non_tool_blocks));
                            if !converted.is_null() {
                                messages.push(json!({"role": "user", "content": converted}));
                            }
                        }
                        continue;
                    }
                }
            }

            if role == "assistant" {
                let mut text_parts = Vec::new();
                let mut thinking_parts = Vec::new();
                let mut tool_calls = Vec::new();

                if let Some(content_arr) = content.as_array() {
                    for block in content_arr {
                        let block_type = block.get("type").and_then(|v| v.as_str());
                        match block_type {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    text_parts.push(text);
                                }
                            }
                            Some("thinking") => {
                                if let Some(thinking) =
                                    block.get("thinking").and_then(|v| v.as_str())
                                {
                                    thinking_parts.push(thinking);
                                }
                            }
                            Some("tool_use") => {
                                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                let openai_id = anthropic_id_to_openai(id);
                                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let name = sanitized_tool_name(name, &original_to_sanitized);
                                let input = block.get("input").cloned().unwrap_or(json!({}));
                                tool_calls.push(json!({
                                    "id": openai_id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": input.to_string()
                                    }
                                }));
                            }
                            Some(other) => {
                                debug!(
                                    "跳过 assistant 消息中不支持的 content block 类型: {}",
                                    other
                                );
                            }
                            None => {}
                        }
                    }
                } else if let Some(text) = content.as_str() {
                    text_parts.push(text);
                }

                let mut msg = json!({"role": "assistant"});
                if !text_parts.is_empty() {
                    msg["content"] = json!(text_parts.join(""));
                } else if tool_calls.is_empty() {
                    msg["content"] = json!(null);
                }
                // When tool_calls present and no text, omit content entirely
                if quirks.reasoning_all_or_nothing || !thinking_parts.is_empty() {
                    msg["reasoning_content"] = json!(thinking_parts.join(""));
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                messages.push(msg);
            } else {
                let converted = convert_content(&content);
                messages.push(json!({"role": role, "content": converted}));
            }
        }
    }

    normalize_tool_call_messages(&mut messages);

    // output_config -> response_format
    let mut json_schema_hint = None;
    if let Some(output_config) = body.get("output_config") {
        if let Some(format_val) = output_config.get("format") {
            let format_type = format_val
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("text");
            match format_type {
                "json_schema" => {
                    if response_format_unavailable(&provider_model) {
                        json_schema_hint = Some(json_schema_instruction(format_val.get("schema")));
                    } else if quirks.no_json_schema {
                        openai["response_format"] = json!({"type": "json_object"});
                        json_schema_hint = Some(json_schema_instruction(format_val.get("schema")));
                    } else {
                        openai["response_format"] = openai_json_schema_response_format(format_val);
                    }
                }
                "json_object" => {
                    if !response_format_unavailable(&provider_model) {
                        openai["response_format"] = json!({"type": "json_object"});
                    }
                    json_schema_hint = Some("Respond with a valid JSON object. Do not include any markdown code fences, explanations, or extra text outside the JSON object.".to_string());
                }
                other => {
                    warn!("未识别的 output_config.format.type: {}，不做转换", other);
                }
            }
        }
    }

    if let Some(hint) = json_schema_hint {
        let mut inserted = false;
        for (i, msg) in messages.iter().enumerate() {
            if msg.get("role").and_then(|v| v.as_str()) != Some("system") {
                messages.insert(i, json!({"role": "system", "content": hint}));
                inserted = true;
                break;
            }
        }
        if !inserted {
            messages.push(json!({"role": "system", "content": hint}));
        }
    }

    openai["messages"] = json!(messages);

    let model_str = provider_model.as_str();
    if let Some(v) = body.get("max_tokens") {
        if is_openai_o_series(model_str) {
            openai["max_completion_tokens"] = v.clone();
        } else {
            openai["max_tokens"] = v.clone();
        }
    }

    if quirks.supports_reasoning_effort || supports_reasoning_effort(model_str) {
        if let Some(effort) = route_reasoning_effort
            .or_else(|| resolve_reasoning_effort(&body, &quirks.max_reasoning_effort))
        {
            openai["reasoning_effort"] = json!(effort);
        }
    }

    for key in ["temperature", "top_p", "top_k", "stream"] {
        if let Some(v) = body.get(key) {
            openai[key] = v.clone();
        }
    }

    if let Some(v) = body.get("stop_sequences") {
        openai["stop"] = v.clone();
    }

    if let Some(tool_choice) = body.get("tool_choice") {
        openai["tool_choice"] = convert_tool_choice(tool_choice, &original_to_sanitized);
    }

    if body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        openai["stream_options"] = json!({"include_usage": true});
    }

    if let Some(thinking) = body.get("thinking") {
        debug!("客户端请求 thinking 配置: {}", thinking);
    }

    (openai, sanitized_to_original)
}

fn normalize_tool_call_messages(messages: &mut Vec<Value>) {
    let mut normalized = Vec::with_capacity(messages.len());
    let mut i = 0;

    while i < messages.len() {
        let mut msg = messages[i].clone();
        if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) else {
                normalized.push(msg);
                i += 1;
                continue;
            };
            if tool_calls.is_empty() {
                normalized.push(msg);
                i += 1;
                continue;
            }

            let requested_ids: Vec<String> = tool_calls
                .iter()
                .filter_map(|call| call.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect();

            let mut j = i + 1;
            let mut following_tools = Vec::new();
            while j < messages.len()
                && messages[j].get("role").and_then(|v| v.as_str()) == Some("tool")
            {
                following_tools.push(messages[j].clone());
                j += 1;
            }

            let mut retained_ids = Vec::new();
            let mut retained_tool_calls = Vec::new();
            for id in &requested_ids {
                if following_tools.iter().any(|tool| {
                    tool.get("tool_call_id").and_then(|v| v.as_str()) == Some(id.as_str())
                }) {
                    if let Some(call) = tool_calls
                        .iter()
                        .find(|call| call.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                    {
                        retained_ids.push(id.clone());
                        retained_tool_calls.push(call.clone());
                    }
                }
            }

            if retained_tool_calls.is_empty() {
                if let Some(obj) = msg.as_object_mut() {
                    obj.remove("tool_calls");
                    if obj.get("content").is_none_or(Value::is_null) {
                        obj.insert("content".to_string(), json!(""));
                    }
                }
                normalized.push(msg);
            } else {
                msg["tool_calls"] = json!(retained_tool_calls);
                normalized.push(msg);
                for id in retained_ids {
                    if let Some(tool) = following_tools.iter().find(|tool| {
                        tool.get("tool_call_id").and_then(|v| v.as_str()) == Some(id.as_str())
                    }) {
                        normalized.push(tool.clone());
                    }
                }
            }

            i = j;
        } else if msg.get("role").and_then(|v| v.as_str()) == Some("tool") {
            i += 1;
        } else {
            normalized.push(msg);
            i += 1;
        }
    }

    *messages = normalized;
}

fn openai_json_schema_response_format(format_val: &Value) -> Value {
    if let Some(json_schema) = format_val.get("json_schema") {
        return json!({
            "type": "json_schema",
            "json_schema": json_schema,
        });
    }

    let name = format_val
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("response_schema");
    let schema = format_val
        .get("schema")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let strict = format_val
        .get("strict")
        .cloned()
        .unwrap_or(Value::Bool(true));

    json!({
        "type": "json_schema",
        "json_schema": {
            "name": name,
            "schema": schema,
            "strict": strict,
        },
    })
}
