//! Content-block, tool, tool-choice conversion and tool-call id mapping
//! shared between the Anthropic↔OpenAI request converters.

use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::debug;

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
                return json!("");
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
