//! OpenAI Chat Completions → Anthropic Messages API request conversion.
//! This is the reverse of `anthropic_to_openai`.

use serde_json::{json, Value};

use super::content::openai_id_to_anthropic;

/// Convert an OpenAI Chat Completions request body to Anthropic Messages API format.
pub(crate) fn openai_to_anthropic(
    body: Value,
    provider: &crate::config::ProviderConfig,
    global_routes: &[crate::config::ModelRoute],
) -> Value {
    let provider_model = provider
        .resolve_model_with_routes(body.get("model").and_then(|v| v.as_str()), global_routes)
        .to_string();

    let mut anthropic = json!({});
    anthropic["model"] = json!(provider_model);

    // Convert tools
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let anthropic_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let func = t.get("function")?;
                let name = func.get("name").and_then(|v| v.as_str())?;
                let mut tool = json!({
                    "type": "tool",
                    "name": name,
                    "input_schema": func.get("parameters").cloned().unwrap_or(json!({}))
                });
                if let Some(desc) = func.get("description").and_then(|v| v.as_str()) {
                    tool["description"] = json!(desc);
                }
                Some(tool)
            })
            .collect();
        anthropic["tools"] = json!(anthropic_tools);
    }

    let mut messages = Vec::new();
    let mut system_blocks: Vec<Value> = Vec::new();

    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        let mut i = 0;
        while i < arr.len() {
            let msg = &arr[i];
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");

            if role == "system" || role == "developer" {
                // Extract system/developer messages into the Anthropic `system` field
                if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                    let mut block = json!({"type": "text", "text": text});
                    if let Some(cc) = msg.get("cache_control") {
                        block["cache_control"] = cc.clone();
                    }
                    system_blocks.push(block);
                }
                i += 1;
                continue;
            }

            if role == "user" {
                if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                    messages.push(json!({"role": "user", "content": text}));
                } else if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
                    let anthropic_blocks: Vec<Value> = arr
                        .iter()
                        .filter_map(|block| {
                            let block_type = block.get("type").and_then(|v| v.as_str());
                            match block_type {
                                Some("text") => {
                                    block.get("text").map(|t| json!({"type": "text", "text": t}))
                                }
                                Some("image_url") => {
                                    // Convert OpenAI image_url back to Anthropic image format
                                    let url = block
                                        .get("image_url")
                                        .and_then(|u| u.get("url"))
                                        .and_then(|u| u.as_str())
                                        .unwrap_or("");
                                    if let Some(data_part) = url.strip_prefix("data:") {
                                        // data:image/png;base64,DATA
                                        if let Some(comma_pos) = data_part.find(',') {
                                            let header = &data_part[..comma_pos];
                                            let data = &data_part[comma_pos + 1..];
                                            let media_type = header
                                                .split(';')
                                                .next()
                                                .unwrap_or("application/octet-stream");
                                            return Some(json!({
                                                "type": "image",
                                                "source": {
                                                    "type": "base64",
                                                    "media_type": media_type,
                                                    "data": data
                                                }
                                            }));
                                        }
                                    } else if url.starts_with("http://") || url.starts_with("https://") {
                                        return Some(json!({
                                            "type": "image",
                                            "source": {
                                                "type": "url",
                                                "url": url
                                            }
                                        }));
                                    }
                                    None
                                }
                                _ => None,
                            }
                        })
                        .collect();
                    if !anthropic_blocks.is_empty() {
                        messages.push(json!({"role": "user", "content": anthropic_blocks}));
                    }
                }
                i += 1;
                continue;
            }

            if role == "assistant" {
                let mut content_blocks = Vec::new();

                // reasoning_content -> thinking block
                if let Some(thinking) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
                    if !thinking.is_empty() {
                        content_blocks.push(json!({
                            "type": "thinking",
                            "thinking": thinking
                        }));
                    }
                }

                // content -> text block
                if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        content_blocks.push(json!({"type": "text", "text": text}));
                    }
                }

                // tool_calls -> tool_use blocks
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for call in tool_calls {
                        let id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let anthropic_id = openai_id_to_anthropic(id);
                        let func = call.get("function").unwrap_or(&Value::Null);
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let arguments = func
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .unwrap_or(json!({}));
                        content_blocks.push(json!({
                            "type": "tool_use",
                            "id": anthropic_id,
                            "name": name,
                            "input": arguments
                        }));
                    }
                }

                if content_blocks.is_empty() {
                    // Empty assistant message
                    messages.push(json!({"role": "assistant", "content": ""}));
                } else {
                    messages.push(json!({"role": "assistant", "content": content_blocks}));
                }
                i += 1;
                continue;
            }

            if role == "tool" {
                // Collect consecutive tool messages into a single user message with tool_result blocks
                let mut tool_results = Vec::new();
                while i < arr.len()
                    && arr[i]
                        .get("role")
                        .and_then(|v| v.as_str())
                        == Some("tool")
                {
                    let tool_msg = &arr[i];
                    let tool_call_id = tool_msg
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let anthropic_id = openai_id_to_anthropic(tool_call_id);
                    // Handle both string and array content
                    let content = match tool_msg.get("content") {
                        Some(Value::String(s)) => Value::String(s.clone()),
                        Some(Value::Array(parts)) => {
                            let blocks: Vec<Value> = parts
                                .iter()
                                .filter_map(|p| {
                                    if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                                        p.get("text").map(|t| json!({"type": "text", "text": t}))
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            if blocks.is_empty() {
                                Value::String(String::new())
                            } else {
                                Value::Array(blocks)
                            }
                        }
                        _ => Value::String(String::new()),
                    };
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": anthropic_id,
                        "content": content
                    }));
                    i += 1;
                }
                if !tool_results.is_empty() {
                    messages.push(json!({"role": "user", "content": tool_results}));
                }
                continue;
            }

            // Other roles: pass through
            if let Some(content) = msg.get("content") {
                messages.push(json!({"role": role, "content": content}));
            }
            i += 1;
        }
    }

    anthropic["messages"] = json!(messages);

    // Set system field
    match system_blocks.len() {
        0 => {}
        1 => {
            anthropic["system"] = system_blocks.remove(0)["text"].clone();
        }
        _ => {
            anthropic["system"] = json!(system_blocks);
        }
    }

    // Parameter mapping
    if let Some(v) = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
    {
        anthropic["max_tokens"] = v.clone();
    }

    for key in ["temperature", "top_p", "top_k", "stream"] {
        if let Some(v) = body.get(key) {
            anthropic[key] = v.clone();
        }
    }

    if let Some(v) = body.get("stop") {
        anthropic["stop_sequences"] = v.clone();
    }

    // tool_choice conversion
    if let Some(tool_choice) = body.get("tool_choice") {
        match tool_choice {
            Value::String(s) => match s.as_str() {
                "auto" => {
                    anthropic["tool_choice"] = json!({"type": "auto"});
                }
                "required" => {
                    anthropic["tool_choice"] = json!({"type": "any"});
                }
                "none" => {
                    anthropic["tool_choice"] = json!({"type": "none"});
                }
                _ => {
                    anthropic["tool_choice"] = json!({"type": "auto"});
                }
            },
            Value::Object(obj) => {
                let choice_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("auto");
                match choice_type {
                    "function" => {
                        let name = obj
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        anthropic["tool_choice"] = json!({"type": "tool", "name": name});
                    }
                    _ => {
                        anthropic["tool_choice"] = json!({"type": "auto"});
                    }
                }
            }
            _ => {
                anthropic["tool_choice"] = json!({"type": "auto"});
            }
        }
    }

    // response_format -> output_config
    if let Some(response_format) = body.get("response_format") {
        let format_type = response_format
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("text");
        match format_type {
            "json_schema" => {
                if let Some(json_schema) = response_format.get("json_schema") {
                    let name = json_schema
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("response");
                    let schema = json_schema.get("schema").cloned().unwrap_or(json!({}));
                    anthropic["output_config"] = json!({
                        "format": {
                            "type": "json_schema",
                            "name": name,
                            "schema": schema
                        }
                    });
                }
            }
            "json_object" => {
                anthropic["output_config"] = json!({
                    "format": {"type": "json_object"}
                });
            }
            _ => {}
        }
    }

    anthropic
}
