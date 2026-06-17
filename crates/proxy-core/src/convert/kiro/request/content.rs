//! Content-block conversion: Anthropic/OpenAI images and tool results → Kiro format.

use serde_json::{json, Value};
use tracing::warn;

pub(super) fn convert_image_block(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    let source_type = source.get("type").and_then(|v| v.as_str()).unwrap_or("base64");

    let (format, data) = if source_type == "url" {
        // URL source - not directly supported by Kiro, skip
        warn!("Kiro 不支持 URL 类型图片，跳过");
        return None;
    } else {
        // Base64 source
        let media_type = source
            .get("media_type")
            .and_then(|v| v.as_str())
            .unwrap_or("image/png");
        let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
        let format = match media_type {
            "image/jpeg" => "jpeg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => {
                warn!(media_type, "不支持的图片格式，跳过");
                return None;
            }
        };
        (format, data)
    };

    Some(json!({
        "format": format,
        "source": {"bytes": data}
    }))
}

/// Convert an OpenAI-format `image_url` block to Kiro image format.
/// Only supports data URLs (e.g., `data:image/jpeg;base64,...`).
pub(super) fn convert_openai_image_block(block: &Value) -> Option<Value> {
    let url = block.get("image_url")?.get("url")?.as_str()?;
    if !url.starts_with("data:") {
        warn!("Kiro 不支持远程 URL 图片，跳过");
        return None;
    }
    let (header, data) = url.split_once(',').unwrap_or(("", ""));
    let format = if header.contains("jpeg") || header.contains("jpg") {
        "jpeg"
    } else if header.contains("png") {
        "png"
    } else if header.contains("gif") {
        "gif"
    } else if header.contains("webp") {
        "webp"
    } else {
        warn!(header, "不支持的图片格式，跳过");
        return None;
    };
    Some(json!({
        "format": format,
        "source": {"bytes": data}
    }))
}

pub(super) fn convert_tool_result(block: &Value) -> (Option<Value>, Vec<Value>) {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let (content, images) = match block.get("content") {
        Some(Value::String(s)) => (vec![json!({"text": s})], vec![]),
        Some(Value::Array(arr)) => {
            let mut text_items = Vec::new();
            let mut extracted_images = Vec::new();
            for b in arr {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text") {
                            text_items.push(json!({"text": t}));
                        }
                    }
                    Some("image") => {
                        if let Some(img) = convert_image_block(b) {
                            extracted_images.push(img);
                        }
                    }
                    Some("image_url") => {
                        if let Some(img) = convert_openai_image_block(b) {
                            extracted_images.push(img);
                        }
                    }
                    _ => {
                        if let Some(s) = b.as_str() {
                            text_items.push(json!({"text": s}));
                        }
                    }
                }
            }
            (text_items, extracted_images)
        }
        _ => (vec![json!({"text": ""})], vec![]),
    };

    let is_error = block
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut result = json!({
        "toolUseId": tool_use_id,
        "content": content,
    });

    if is_error {
        result["status"] = json!("error");
        result["isError"] = json!(true);
    } else {
        result["status"] = json!("success");
    }

    (Some(result), images)
}
