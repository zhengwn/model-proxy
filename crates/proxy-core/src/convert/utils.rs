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
