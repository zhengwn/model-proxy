//! Tool conversion: Anthropic tool specs → Kiro tool format, name shortening,
//! and JSON Schema normalization for Kiro API compatibility.

use serde_json::{json, Value};
use std::collections::HashMap;

/// Maximum tool name length in Kiro API (characters, not bytes).
const TOOL_NAME_MAX_LEN: usize = 63;

/// Maximum tool description length.
const TOOL_DESC_MAX_LEN: usize = 10_000;

/// Convert Anthropic tools to Kiro tool format.
/// Returns `(kiro_tools, tool_documentation)` where `tool_documentation` contains
/// full descriptions of tools that exceeded the length limit.
pub(super) fn convert_tools(tools: &[Value], name_map: &mut HashMap<String, String>) -> (Value, String) {
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
pub(super) fn shorten_tool_name(name: &str) -> (String, bool) {
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
pub(super) fn normalize_json_schema(mut schema: Value) -> Value {
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
