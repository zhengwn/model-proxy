//! Tool input transformations for Kiro compatibility.
//!
//! Converts structured tool inputs into formats expected by the Kiro API.

use serde_json::Value;

/// Transform known tool inputs before sending to Kiro.
/// Returns true if the tool was transformed.
pub fn transform_tool_input(tool_name: &str, input: &mut Value) -> bool {
    match tool_name {
        "TodoWrite" => transform_todo_write(input),
        _ => false,
    }
}

/// Transform TodoWrite structured todos array into a formatted text list.
/// Converts: [{status: "in_progress", content: "task1", activeForm: "working"}]
/// To: "1. [in_progress] task1 (working)"
fn transform_todo_write(input: &mut Value) -> bool {
    let todos = match input.get_mut("todos").and_then(|v| v.as_array_mut()) {
        Some(arr) => arr,
        None => return false,
    };

    let formatted: Vec<String> = todos
        .iter()
        .enumerate()
        .map(|(i, todo)| {
            let status = todo
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            let content = todo
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let active_form = todo.get("activeForm").and_then(|v| v.as_str());

            match active_form {
                Some(form) => format!("{}. [{}] {} ({})", i + 1, status, content, form),
                None => format!("{}. [{}] {}", i + 1, status, content),
            }
        })
        .collect();

    input["todos"] = serde_json::json!(formatted.join("\n"));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transform_todo_write_basic() {
        let mut input = json!({
            "todos": [
                {"status": "in_progress", "content": "Build feature", "activeForm": "Building"},
                {"status": "completed", "content": "Write tests"}
            ]
        });
        assert!(transform_todo_write(&mut input));
        let result = input["todos"].as_str().unwrap();
        assert!(result.contains("1. [in_progress] Build feature (Building)"));
        assert!(result.contains("2. [completed] Write tests"));
    }

    #[test]
    fn transform_todo_write_no_active_form() {
        let mut input = json!({
            "todos": [
                {"status": "pending", "content": "Task one"},
                {"content": "Task two"}
            ]
        });
        assert!(transform_todo_write(&mut input));
        let result = input["todos"].as_str().unwrap();
        assert!(result.contains("1. [pending] Task one"));
        assert!(result.contains("2. [pending] Task two"));
    }

    #[test]
    fn transform_todo_write_no_todos() {
        let mut input = json!({});
        assert!(!transform_todo_write(&mut input));
    }

    #[test]
    fn transform_tool_input_unknown() {
        let mut input = json!({"foo": "bar"});
        assert!(!transform_tool_input("UnknownTool", &mut input));
    }

    #[test]
    fn transform_tool_input_todo_write() {
        let mut input = json!({"todos": [{"content": "task"}]});
        assert!(transform_tool_input("TodoWrite", &mut input));
    }
}
