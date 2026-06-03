use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::{debug, warn};

use crate::server::state::strip_leading_anthropic_billing_header;
use crate::config::ProviderFormat;
use crate::error::Result;

/// Heuristic: detect OpenAI o-series reasoning models (o1, o3, o4-mini, etc.).
/// Matches any model name starting with 'o' followed by a digit.
/// This may produce false positives for custom model names; use `quirks.supports_reasoning_effort`
/// to override behavior for non-OpenAI providers.
pub(crate) fn is_openai_o_series(model: &str) -> bool {
    model.len() > 1
        && model.starts_with('o')
        && model.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit())
}

/// Heuristic: detect models that support the `reasoning_effort` parameter.
/// Covers OpenAI o-series and GPT-5+. For non-OpenAI models, use
/// `quirks.supports_reasoning_effort = true` in the provider config instead.
pub(crate) fn supports_reasoning_effort(model: &str) -> bool {
    is_openai_o_series(model)
        || model
            .to_lowercase()
            .strip_prefix("gpt-")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_digit() && c >= '5')
}

fn response_format_unavailable(model: &str) -> bool {
    model.to_ascii_lowercase().contains("deepseek")
}

fn json_schema_instruction(schema: Option<&Value>) -> String {
    match schema.and_then(|schema| serde_json::to_string(schema).ok()) {
        Some(schema_str) => format!(
            "You must respond with a valid JSON object that strictly conforms to the following JSON Schema. Do not include any markdown code fences, explanations, or extra text outside the JSON object.\n\nSchema:\n{}",
            schema_str
        ),
        None => "Respond with a valid JSON object. Do not include any markdown code fences, explanations, or extra text outside the JSON object.".to_string(),
    }
}

pub(crate) fn resolve_reasoning_effort(body: &Value, max_effort: &str) -> Option<String> {
    if let Some(effort) = body
        .pointer("/output_config/effort")
        .and_then(|v| v.as_str())
    {
        return match effort {
            "low" => Some("low".into()),
            "medium" => Some("medium".into()),
            "high" => Some("high".into()),
            "max" => Some(max_effort.to_string()),
            _ => None,
        };
    }

    let thinking = body.get("thinking")?;
    match thinking.get("type").and_then(|t| t.as_str()) {
        Some("adaptive") => Some(max_effort.to_string()),
        Some("enabled") => {
            let budget = thinking
                .get("budget_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            match budget {
                0 => Some("high".into()),
                1..=3999 => Some("low".into()),
                4000..=15999 => Some("medium".into()),
                _ => Some("high".into()),
            }
        }
        Some("disabled") => None,
        _ => None,
    }
}

pub(crate) fn convert_content(content: &Value) -> Value {
    match content {
        Value::String(s) => json!(s),
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for block in arr {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            let mut part = json!({
                                "type": "text",
                                "text": text
                            });
                            if let Some(cc) = block.get("cache_control") {
                                part["cache_control"] = cc.clone();
                            }
                            parts.push(part);
                        }
                    }
                    "image" => {
                        if let Some(source) = block.get("source") {
                            let source_type = source.get("type").and_then(|v| v.as_str()).unwrap_or("base64");
                            let url = if source_type == "url" {
                                source.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string()
                            } else {
                                // base64 source
                                let media_type = source
                                    .get("media_type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("image/png");
                                let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
                                format!("data:{};base64,{}", media_type, data)
                            };
                            if !url.is_empty() {
                                parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {"url": url}
                                }));
                            }
                        }
                    }
                    other => {
                        debug!("跳过不支持的 content block 类型: {}", other);
                    }
                }
            }
            if parts.is_empty() {
                return json!(null);
            }
            if parts.len() == 1 {
                let has_cache_control = parts[0].get("cache_control").is_some();
                if !has_cache_control {
                    if let Some(text) = parts[0].get("text").and_then(|v| v.as_str()) {
                        return json!(text);
                    }
                }
            }
            json!(parts)
        }
        _ => json!(null),
    }
}

pub(crate) fn sanitize_tool_name(
    name: &str,
    existing: &std::collections::HashSet<String>,
) -> (String, bool) {
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() > 64 {
        sanitized.truncate(64);
    }
    let base = sanitized.clone();
    let mut counter = 2;
    while existing.contains(&sanitized) {
        let suffix = format!("_{}", counter);
        let max_base_len = 64usize.saturating_sub(suffix.len());
        let truncate_to = base.len().min(max_base_len);
        sanitized = format!("{}{}", &base[..truncate_to], suffix);
        counter += 1;
    }
    let modified = sanitized != name;
    (sanitized, modified)
}

pub fn clean_schema(schema: &mut Value) {
    if let Some(obj) = schema.as_object_mut() {
        if obj.get("format").and_then(|v| v.as_str()) == Some("uri") {
            obj.remove("format");
        }
        if let Some(properties) = obj.get_mut("properties").and_then(|v| v.as_object_mut()) {
            for value in properties.values_mut() {
                clean_schema(value);
            }
        }
        if let Some(items) = obj.get_mut("items") {
            clean_schema(items);
        }
    }
}

pub(crate) fn convert_tools(
    tools: &Value,
    sanitized_to_original: &mut HashMap<String, String>,
) -> Value {
    use std::collections::HashSet;
    match tools.as_array() {
        Some(arr) => {
            let mut existing = HashSet::new();
            let openai_tools: Vec<Value> = arr
                .iter()
                .filter(|t| t.get("type").and_then(|v| v.as_str()) != Some("BatchTool"))
                .map(|tool| {
                    let raw_name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let (name, modified) = sanitize_tool_name(raw_name, &existing);
                    existing.insert(name.clone());
                    if modified {
                        sanitized_to_original.insert(name.clone(), raw_name.to_string());
                    }
                    let description = tool
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let mut input_schema = tool.get("input_schema").cloned().unwrap_or(json!({}));
                    clean_schema(&mut input_schema);
                    let mut t = json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description,
                            "parameters": input_schema
                        }
                    });
                    if let Some(cc) = tool.get("cache_control") {
                        t["cache_control"] = cc.clone();
                    }
                    t
                })
                .collect();
            json!(openai_tools)
        }
        None => json!([]),
    }
}

pub(crate) fn sanitized_tool_name<'a>(
    name: &'a str,
    original_to_sanitized: &'a HashMap<String, String>,
) -> &'a str {
    original_to_sanitized
        .get(name)
        .map(|s| s.as_str())
        .unwrap_or(name)
}

pub(crate) fn convert_tool_choice(
    tool_choice: &Value,
    original_to_sanitized: &HashMap<String, String>,
) -> Value {
    match tool_choice {
        Value::String(s) => match s.as_str() {
            "auto" => json!("auto"),
            "any" => json!("required"),
            "none" => json!("none"),
            _ => json!("auto"),
        },
        Value::Object(obj) => {
            let choice_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("auto");
            match choice_type {
                "auto" => json!("auto"),
                "any" => json!("required"),
                "none" => json!("none"),
                "tool" => {
                    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let name = sanitized_tool_name(name, original_to_sanitized);
                    json!({
                        "type": "function",
                        "function": {
                            "name": name
                        }
                    })
                }
                _ => json!("auto"),
            }
        }
        _ => json!("auto"),
    }
}

pub(crate) fn anthropic_id_to_openai(id: &str) -> String {
    if let Some(stripped) = id.strip_prefix("toolu_") {
        format!("call_{}", stripped)
    } else {
        format!("call_{}", id)
    }
}

pub(crate) fn openai_id_to_anthropic(id: &str) -> String {
    if let Some(stripped) = id.strip_prefix("call_") {
        format!("toolu_{}", stripped)
    } else {
        format!("toolu_{}", id)
    }
}

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

/// Convert an OpenAI Chat Completions request body to Anthropic Messages API format.
/// This is the reverse of `anthropic_to_openai`.
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

/// Prepare body for `/v1/chat/completions` endpoint (OpenAI-format input).
/// Mirrors `prepare_body()` but accepts OpenAI Chat Completions format.
pub(crate) fn prepare_chat_completions_body(
    mut body: Value,
    provider: &crate::config::ProviderConfig,
    global_routes: &[crate::config::ModelRoute],
) -> Result<(Value, bool, HashMap<String, String>)> {
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match provider.format {
        ProviderFormat::Openai => {
            // Already in OpenAI format — just resolve model name
            if let Some(obj) = body.as_object_mut() {
                let provider_model = provider
                    .resolve_model_with_routes(
                        obj.get("model").and_then(|v| v.as_str()),
                        global_routes,
                    )
                    .to_string();
                obj.insert("model".to_string(), json!(provider_model));
            }
            Ok((body, stream, HashMap::new()))
        }
        ProviderFormat::Anthropic => {
            let anthropic_body = openai_to_anthropic(body, provider, global_routes);
            Ok((anthropic_body, stream, HashMap::new()))
        }
        ProviderFormat::Kiro => {
            // Kiro 转换由 convert/kiro/request.rs 处理
            Err(crate::error::AppError::Request(
                "Kiro format should use prepare_kiro_body() instead".to_string(),
            ))
        }
    }
}

pub(crate) fn prepare_body(
    mut body: Value,
    provider: &crate::config::ProviderConfig,
    global_routes: &[crate::config::ModelRoute],
) -> Result<(Value, bool, HashMap<String, String>)> {
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match provider.format {
        ProviderFormat::Openai => {
            let (openai_body, tool_name_map) = anthropic_to_openai(body, provider, global_routes);
            Ok((openai_body, stream, tool_name_map))
        }
        ProviderFormat::Anthropic => {
            if let Some(obj) = body.as_object_mut() {
                let provider_model = provider
                    .resolve_model_with_routes(
                        obj.get("model").and_then(|v| v.as_str()),
                        global_routes,
                    )
                    .to_string();
                obj.insert("model".to_string(), json!(provider_model));
            }
            Ok((body, stream, HashMap::new()))
        }
        ProviderFormat::Kiro => {
            // Kiro 转换由 convert/kiro/request.rs 处理，此路径不应到达
            Err(crate::error::AppError::Request(
                "Kiro format should use prepare_kiro_body() instead of prepare_body()".to_string(),
            ))
        }
    }
}

pub(crate) fn build_provider_request(
    client: &reqwest::Client,
    provider: &crate::config::ProviderConfig,
    body: Value,
) -> reqwest::RequestBuilder {
    match provider.format {
        ProviderFormat::Openai => {
            let url = openai_chat_completions_url(&provider.base_url);
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", provider.api_key))
                .json(&body)
        }
        ProviderFormat::Anthropic => {
            let url = anthropic_messages_url(&provider.base_url);
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-api-key", provider.api_key.as_str())
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        }
        ProviderFormat::Kiro => {
            // Kiro 请求构建由 convert/kiro 模块处理，此为占位
            // 实际的 Kiro 请求需要特殊 headers（x-amzn-kiro-agent-mode 等）
            let url = format!(
                "https://q.{region}.amazonaws.com/generateAssistantResponse",
                region = provider
                    .kiro_config
                    .as_ref()
                    .and_then(|k| k.api_region.as_deref())
                    .or(provider.kiro_config.as_ref().map(|k| k.region.as_str()))
                    .unwrap_or("us-east-1")
            );
            client
                .post(&url)
                .header("content-type", "application/json")
                .json(&body)
        }
    }
}

pub fn openai_chat_completions_url(base_url: &str) -> String {
    let base_url = base_url.trim();
    if path_has_suffix(base_url, &["chat", "completions"]) {
        return base_url.to_string();
    }

    let endpoint = if has_openai_api_prefix(base_url) {
        "chat/completions"
    } else {
        "v1/chat/completions"
    };
    append_endpoint(base_url, endpoint)
}

pub fn anthropic_messages_url(base_url: &str) -> String {
    let base_url = base_url.trim();
    if path_has_suffix(base_url, &["messages"]) {
        return base_url.to_string();
    }

    let endpoint = if path_segments(base_url)
        .iter()
        .any(|segment| segment == "v1")
    {
        "messages"
    } else {
        "v1/messages"
    };
    append_endpoint(base_url, endpoint)
}

fn append_endpoint(base_url: &str, endpoint: &str) -> String {
    let (path_part, suffix) = split_url_suffix(base_url.trim_end_matches('/'));
    format!("{}/{}{}", path_part.trim_end_matches('/'), endpoint, suffix)
}

fn has_openai_api_prefix(base_url: &str) -> bool {
    path_segments(base_url)
        .iter()
        .any(|segment| segment == "openai" || segment == "v1" || segment.starts_with("v1beta"))
}

fn path_has_suffix(base_url: &str, suffix: &[&str]) -> bool {
    let segments = path_segments(base_url);
    segments.len() >= suffix.len()
        && segments[segments.len() - suffix.len()..]
            .iter()
            .zip(suffix.iter())
            .all(|(segment, expected)| segment == expected)
}

fn path_segments(base_url: &str) -> Vec<String> {
    let (without_suffix, _) = split_url_suffix(base_url);
    let path = if let Some((_, rest)) = without_suffix.split_once("://") {
        rest.find('/').map(|idx| &rest[idx..]).unwrap_or("")
    } else {
        without_suffix
    };

    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_ascii_lowercase())
        .collect()
}

fn split_url_suffix(url: &str) -> (&str, &str) {
    let suffix_index = url
        .char_indices()
        .find(|(_, ch)| *ch == '?' || *ch == '#')
        .map(|(idx, _)| idx);

    match suffix_index {
        Some(idx) => (&url[..idx], &url[idx..]),
        None => (url, ""),
    }
}

#[cfg(test)]
mod url_tests {
    use super::{anthropic_messages_url, openai_chat_completions_url};

    #[test]
    fn openai_url_adds_v1_for_host_root() {
        assert_eq!(
            openai_chat_completions_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
    }

    #[test]
    fn openai_url_does_not_duplicate_v1_prefix() {
        assert_eq!(
            openai_chat_completions_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn openai_url_preserves_openai_compatible_vendor_prefix() {
        assert_eq!(
            openai_chat_completions_url("https://generativelanguage.googleapis.com/v1beta/openai"),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
    }

    #[test]
    fn openai_url_preserves_query_string_after_endpoint() {
        assert_eq!(
            openai_chat_completions_url(
                "https://example.openai.azure.com/openai/deployments/demo?api-version=2024-10-21"
            ),
            "https://example.openai.azure.com/openai/deployments/demo/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn anthropic_url_does_not_duplicate_v1_prefix() {
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
    }
}
