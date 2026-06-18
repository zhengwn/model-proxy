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
/// For MCP-style names (e.g. `mcp_server_filesystem_read_file`), tries extracting
/// the last segment first, then falls back to hash-based truncation.
/// Returns (shortened_name, was_shortened).
pub(super) fn shorten_tool_name(name: &str) -> (String, bool) {
    if name.chars().count() <= TOOL_NAME_MAX_LEN {
        return (name.to_string(), false);
    }

    // First attempt: extract last segment for MCP-style names
    // (e.g. "mcp_server_filesystem_read_file" → "read_file")
    if let Some(last_segment) = extract_last_segment(name) {
        if last_segment.chars().count() <= TOOL_NAME_MAX_LEN && !last_segment.is_empty() {
            return (last_segment.to_string(), true);
        }
    }

    // Second attempt: hash-based shortening
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

/// Extract the last segment from an underscore or slash-separated tool name.
/// "mcp_server_fs_read_file" → Some("read_file")
/// "namespace/tool/action" → Some("action")
fn extract_last_segment(name: &str) -> Option<&str> {
    // Try underscore separator first (most common for MCP)
    if let Some(pos) = name.rfind('_') {
        let last = &name[pos + 1..];
        if !last.is_empty() {
            return Some(last);
        }
    }
    // Try slash separator
    if let Some(pos) = name.rfind('/') {
        let last = &name[pos + 1..];
        if !last.is_empty() {
            return Some(last);
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_tool_name_no_change() {
        let (name, shortened) = shorten_tool_name("read_file");
        assert_eq!(name, "read_file");
        assert!(!shortened);
    }

    #[test]
    fn shorten_mcp_tool_name() {
        // Name over 63 chars with underscore separator
        let name = "mcp_very_long_server_name_that_exceeds_the_limit_and_more_chars_read_file";
        let (result, shortened) = shorten_tool_name(name);
        assert!(shortened);
        // Last segment after final '_' is "file"
        assert_eq!(result, "file");
    }

    #[test]
    fn shorten_mcp_tool_name_slash() {
        // Name over 63 chars with slash separator
        let name = "mcp/very/long/server/name/that/exceeds/the/limit/and/has/many/segments/action";
        let (result, shortened) = shorten_tool_name(name);
        assert!(shortened);
        assert_eq!(result, "action");
    }

    #[test]
    fn shorten_tool_name_hash_fallback() {
        // Name that exceeds limit but has no underscore/slash separators
        let long_name = "a".repeat(100);
        let (name, shortened) = shorten_tool_name(&long_name);
        assert!(shortened);
        assert!(name.chars().count() <= TOOL_NAME_MAX_LEN);
    }

    #[test]
    fn extract_last_segment_underscore() {
        assert_eq!(extract_last_segment("mcp_server_fs_read"), Some("read"));
        assert_eq!(extract_last_segment("a_b_c"), Some("c"));
    }

    #[test]
    fn extract_last_segment_slash() {
        assert_eq!(extract_last_segment("a/b/c"), Some("c"));
    }

    #[test]
    fn extract_last_segment_no_separator() {
        assert_eq!(extract_last_segment("simple"), None);
    }
}
