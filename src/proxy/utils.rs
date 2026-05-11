use serde_json::Value;

pub(crate) fn append_utf8_safe(buffer: &mut String, remainder: &mut Vec<u8>, bytes: &[u8]) {
    remainder.extend_from_slice(bytes);
    match String::from_utf8(std::mem::take(remainder)) {
        Ok(text) => {
            buffer.push_str(&text);
        }
        Err(e) => {
            let valid = e.utf8_error().valid_up_to();
            let bytes = e.into_bytes();
            buffer.push_str(&String::from_utf8_lossy(&bytes[..valid]));
            *remainder = bytes[valid..].to_vec();
        }
    }
}

pub(crate) fn find_sse_block_end(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|pos| (pos, 2));
    let crlf = buffer.find("\r\n\r\n").map(|pos| (pos, 4));

    match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(if lf.0 < crlf.0 { lf } else { crlf }),
        (Some(lf), None) => Some(lf),
        (None, Some(crlf)) => Some(crlf),
        (None, None) => None,
    }
}

pub(crate) fn message_count(body: &Value) -> usize {
    body.get("messages")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0)
}

pub(crate) fn tool_count(body: &Value) -> usize {
    body.get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0)
}

pub(crate) fn truncate_for_log(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}...[truncated {} bytes]",
        &text[..end],
        text.len().saturating_sub(end)
    )
}

pub(crate) fn estimate_input_tokens(body: &Value) -> u64 {
    fn estimate_json_chars(value: &Value) -> usize {
        match value {
            Value::Null => 4,
            Value::Bool(_) => 5,
            Value::Number(n) => n.to_string().len(),
            Value::String(s) => s.len(),
            Value::Array(arr) => {
                2 + arr
                    .iter()
                    .map(estimate_json_chars)
                    .sum::<usize>()
                    .saturating_add(arr.len().saturating_sub(1))
            }
            Value::Object(obj) => {
                2 + obj
                    .iter()
                    .map(|(key, value)| key.len() + estimate_json_chars(value) + 3)
                    .sum::<usize>()
                    .saturating_add(obj.len().saturating_sub(1))
            }
        }
    }

    let msg_chars: usize = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .map(|msg| {
                    msg.get("content")
                        .map(|c| match c {
                            Value::String(s) => s.len(),
                            Value::Array(a) => a
                                .iter()
                                .map(|item| {
                                    let mut chars = 0usize;
                                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                        chars += text.len();
                                    }
                                    if let Some(thinking) =
                                        item.get("thinking").and_then(|t| t.as_str())
                                    {
                                        chars += thinking.len();
                                    }
                                    if let Some(content) = item.get("content") {
                                        if let Some(s) = content.as_str() {
                                            chars += s.len();
                                        } else if let Some(arr) = content.as_array() {
                                            chars += arr
                                                .iter()
                                                .filter_map(|b| {
                                                    b.get("text")
                                                        .and_then(|t| t.as_str())
                                                        .map(|s| s.len())
                                                })
                                                .sum::<usize>();
                                        }
                                    }
                                    if let Some(name) = item.get("name").and_then(|t| t.as_str()) {
                                        chars += name.len();
                                    }
                                    if let Some(input) = item.get("input") {
                                        chars += estimate_json_chars(input);
                                    }
                                    chars
                                })
                                .sum(),
                            _ => 0,
                        })
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0);

    let system_chars: usize = body
        .get("system")
        .map(|s| match s {
            Value::String(text) => text.len(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(|s| s.len()))
                .sum(),
            _ => 0,
        })
        .unwrap_or(0);

    let tools_chars: usize = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| arr.iter().map(estimate_json_chars).sum())
        .unwrap_or(0);

    ((msg_chars + system_chars + tools_chars) / 3) as u64
}
