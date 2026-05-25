use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::{debug, warn};

use super::state::strip_leading_anthropic_billing_header;
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
                            let media_type = source
                                .get("media_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("image/png");
                            let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
                            let url = format!("data:{};base64,{}", media_type, data);
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {"url": url}
                            }));
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
            _ => json!("auto"),
        },
        Value::Object(obj) => {
            let choice_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("auto");
            match choice_type {
                "auto" => json!("auto"),
                "any" => json!("required"),
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
                                            .filter_map(|b| {
                                                b.get("text")
                                                    .and_then(|t| t.as_str())
                                                    .map(|s| s.to_string())
                                            })
                                            .collect();
                                        texts.join("")
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
                if text_parts.is_empty() {
                    msg["content"] = json!(null);
                } else {
                    msg["content"] = json!(text_parts.join(""));
                }
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
                    if quirks.no_json_schema {
                        openai["response_format"] = json!({"type": "json_object"});
                        if let Some(schema) = format_val.get("schema") {
                            if let Ok(schema_str) = serde_json::to_string(schema) {
                                json_schema_hint = Some(format!(
                                    "You must respond with a valid JSON object that strictly conforms to the following JSON Schema. Do not include any markdown code fences, explanations, or extra text outside the JSON object.\n\nSchema:\n{}",
                                    schema_str
                                ));
                            }
                        }
                    } else {
                        openai["response_format"] = format_val.clone();
                    }
                }
                "json_object" => {
                    openai["response_format"] = json!({"type": "json_object"});
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
    }
}

pub(crate) fn build_provider_request(
    client: &reqwest::Client,
    provider: &crate::config::ProviderConfig,
    body: Value,
) -> reqwest::RequestBuilder {
    match provider.format {
        ProviderFormat::Openai => {
            let url = format!(
                "{}/v1/chat/completions",
                provider.base_url.trim_end_matches('/')
            );
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", provider.api_key))
                .json(&body)
        }
        ProviderFormat::Anthropic => {
            let url = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-api-key", provider.api_key.as_str())
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        }
    }
}
