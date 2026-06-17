use axum::body::Body;
use axum::http::header;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;
use tracing::{error, info};

use super::request::{anthropic_id_to_openai, openai_id_to_anthropic};
use crate::server::state::{elapsed_ms, MAX_LOG_BODY_BYTES};
use super::stream::{build_anthropic_usage, extract_openai_usage_parts};
use crate::convert::utils::truncate_for_log;
use crate::error::{AppError, Result};

pub(crate) async fn handle_non_stream(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_reverse_map: &HashMap<String, String>,
    request_id: &str,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
) -> Result<Response> {
    let content_type = upstream_resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let is_json = media_type == "application/json" || media_type.ends_with("+json");

    let body_bytes = upstream_resp.bytes().await?;
    let body: Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        let body_preview = String::from_utf8_lossy(&body_bytes);
        let preview = truncate_for_log(&body_preview, MAX_LOG_BODY_BYTES);
        if is_json {
            error!(
                request_id,
                content_type = %content_type,
                body_preview = %preview,
                error = %e,
                "上游返回了 Content-Type: application/json 但 JSON 解析失败"
            );
        } else {
            error!(
                request_id,
                content_type = %content_type,
                body_preview = %preview,
                "上游返回了非 JSON 响应"
            );
        }
        AppError::UpstreamInvalidResponse(format!(
            "content-type={}, parse error: {}",
            content_type, e
        ))
    })?;
    info!(
        request_id,
        response_id = body.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        choices = body
            .get("choices")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len())
            .unwrap_or(0),
        has_usage = body.get("usage").is_some(),
        upstream_headers_ms,
        upstream_total_ms = elapsed_ms(upstream_start),
        request_total_ms = elapsed_ms(request_start),
        "上游非流式响应"
    );

    let anthropic = convert_non_stream_response(body, model, tool_name_reverse_map);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(anthropic.to_string()))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))
}

pub(crate) fn convert_non_stream_response(
    body: Value,
    model: &str,
    tool_name_reverse_map: &HashMap<String, String>,
) -> Value {
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("msg_00000000000000000000")
        .to_string();

    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .cloned()
        .unwrap_or(json!({}));

    let message = choice.get("message").cloned().unwrap_or(json!({}));

    let mut content_blocks = Vec::new();
    let mut has_tool_use = false;

    if let Some(reasoning) = message.get("reasoning_content").and_then(|v| v.as_str()) {
        if !reasoning.is_empty() {
            content_blocks.push(json!({
                "type": "thinking",
                "thinking": reasoning,
                "signature": ""
            }));
        }
    }

    if let Some(msg_content) = message.get("content") {
        if let Some(text) = msg_content.as_str() {
            if !text.is_empty() {
                content_blocks.push(json!({
                    "type": "text",
                    "text": text
                }));
            }
        } else if let Some(parts) = msg_content.as_array() {
            for part in parts {
                let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match part_type {
                    "text" | "output_text" => {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                content_blocks.push(json!({"type": "text", "text": text}));
                            }
                        }
                    }
                    "refusal" => {
                        if let Some(refusal) = part.get("refusal").and_then(|r| r.as_str()) {
                            if !refusal.is_empty() {
                                content_blocks.push(json!({"type": "text", "text": refusal}));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(refusal) = message.get("refusal").and_then(|r| r.as_str()) {
        if !refusal.is_empty() {
            content_blocks.push(json!({"type": "text", "text": refusal}));
        }
    }

    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        if !tool_calls.is_empty() {
            has_tool_use = true;
        }
        for call in tool_calls {
            let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let function = call.get("function").cloned().unwrap_or(json!({}));
            let name = function.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let original_name = tool_name_reverse_map
                .get(name)
                .map(|s| s.as_str())
                .unwrap_or(name);
            let arguments = function
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));

            let anthropic_id = openai_id_to_anthropic(call_id);

            content_blocks.push(json!({
                "type": "tool_use",
                "id": anthropic_id,
                "name": original_name,
                "input": args
            }));
        }
    }

    if !has_tool_use {
        if let Some(function_call) = message.get("function_call") {
            let id = function_call
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("");
            let name = function_call
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let has_arguments = function_call.get("arguments").is_some();
            let input = match function_call.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!({})),
                Some(v @ Value::Object(_)) | Some(v @ Value::Array(_)) => v.clone(),
                _ => json!({}),
            };
            if !name.is_empty() || has_arguments {
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": openai_id_to_anthropic(id),
                    "name": name,
                    "input": input
                }));
                has_tool_use = true;
            }
        }
    }

    let finish_reason = choice.get("finish_reason").and_then(|v| v.as_str());

    let stop_reason = match finish_reason {
        Some("stop") => "end_turn",
        Some("length") => "max_tokens",
        Some("content_filter") => "refusal",
        Some("tool_calls") | Some("function_call") => "tool_use",
        _ => {
            if has_tool_use {
                "tool_use"
            } else {
                "end_turn"
            }
        }
    };

    let usage = body.get("usage").cloned().unwrap_or(json!({}));
    let usage_parts = extract_openai_usage_parts(&usage);

    json!({
        "type": "message",
        "id": id,
        "role": "assistant",
        "content": content_blocks,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": build_anthropic_usage(usage_parts, 0)
    })
}

/// Convert Anthropic usage fields to OpenAI format.
fn build_openai_usage(usage: &Value) -> Value {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64());
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64());

    let mut result = json!({
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "total_tokens": input_tokens + output_tokens,
    });

    // Include cache info if present
    if cache_read.is_some() || cache_creation.is_some() {
        let mut details = json!({});
        if let Some(cr) = cache_read {
            details["cached_tokens"] = json!(cr);
        }
        if let Some(cc) = cache_creation {
            details["cache_creation_input_tokens"] = json!(cc);
        }
        result["prompt_tokens_details"] = details;
        // Also include as top-level keys for broader compatibility
        if let Some(cr) = cache_read {
            result["prompt_cache_hit_tokens"] = json!(cr);
        }
    }

    result
}

/// Convert an Anthropic Messages API response to OpenAI Chat Completions format.
pub(crate) fn convert_anthropic_to_openai_response(body: Value, model: &str) -> Value {
    let msg_id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("msg_00000000000000000000");
    let chat_id = format!("chatcmpl-{}", msg_id);

    let content_blocks = body
        .get("content")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text_content: Option<String> = None;
    let mut reasoning_content: Option<String> = None;
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in &content_blocks {
        let block_type = block.get("type").and_then(|v| v.as_str());
        match block_type {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    let entry = text_content.get_or_insert_with(String::new);
                    entry.push_str(text);
                }
            }
            Some("thinking") => {
                if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
                    let entry = reasoning_content.get_or_insert_with(String::new);
                    entry.push_str(thinking);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let openai_id = anthropic_id_to_openai(id);
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or(json!({}));
                let arguments_str = match &input {
                    Value::String(s) => s.clone(),
                    _ => input.to_string(),
                };
                tool_calls.push(json!({
                    "id": openai_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments_str
                    }
                }));
            }
            _ => {}
        }
    }

    // Build message object
    let mut message = json!({
        "role": "assistant",
        "content": text_content.map_or(Value::Null, Value::String),
    });

    if let Some(rc) = reasoning_content {
        message["reasoning_content"] = json!(rc);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    // Map stop_reason
    let stop_reason = body
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    let finish_reason = match stop_reason {
        "end_turn" | "pause_turn" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    };

    let anthropic_usage = body.get("usage").cloned().unwrap_or(json!({}));
    let openai_usage = build_openai_usage(&anthropic_usage);

    json!({
        "id": chat_id,
        "object": "chat.completion",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }],
        "usage": openai_usage
    })
}

/// Handle a non-streaming Anthropic upstream response, converting it to OpenAI format.
pub(crate) async fn handle_non_stream_openai_output(
    upstream_resp: reqwest::Response,
    model: &str,
    request_id: &str,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
) -> Result<Response> {
    let body_bytes = upstream_resp.bytes().await?;
    let body: Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        let preview = String::from_utf8_lossy(&body_bytes);
        let preview = truncate_for_log(&preview, MAX_LOG_BODY_BYTES);
        error!(
            request_id,
            body_preview = %preview,
            error = %e,
            "上游 Anthropic 响应 JSON 解析失败"
        );
        AppError::UpstreamInvalidResponse(format!("parse error: {}", e))
    })?;

    info!(
        request_id,
        response_id = body.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        has_usage = body.get("usage").is_some(),
        upstream_headers_ms,
        upstream_total_ms = elapsed_ms(upstream_start),
        request_total_ms = elapsed_ms(request_start),
        "上游 Anthropic 非流式响应，转换为 OpenAI 格式"
    );

    let openai_response = convert_anthropic_to_openai_response(body, model);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(openai_response.to_string()))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))
}
