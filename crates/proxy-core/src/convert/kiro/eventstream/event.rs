//! High-level Kiro events decoded from EventStream frames (plus a text fallback).

use serde::Deserialize;
use serde_json::Value;

use super::{Frame, ParseError, ParseResult};

// ---- Event types ----

/// High-level events parsed from Kiro API EventStream frames.
#[derive(Debug, Clone)]
pub enum Event {
    /// Text content from the assistant
    AssistantResponse { content: String },
    /// Reasoning/thinking content
    ReasoningContent { text: String },
    /// Tool use invocation (incremental)
    ToolUse {
        name: String,
        tool_use_id: String,
        input: String,
        stop: bool,
    },
    /// Context window usage percentage
    ContextUsage { percentage: f64 },
    /// Billing/credit usage
    Metering { usage: f64 },
    /// Token usage metadata (direct token counts from Kiro)
    MessageMetadata {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    },
    /// Error from the API
    Error { code: String, message: String },
    /// Exception from the API
    Exception {
        type_name: String,
        message: String,
    },
    /// Unrecognized event type
    Unknown,
}

// ---- Event payload structs for JSON deserialization ----

#[derive(serde::Deserialize)]
struct AssistantResponsePayload {
    content: String,
}

#[derive(serde::Deserialize)]
struct ReasoningContentPayload {
    text: String,
}

/// Deserialize `input` field that may be a JSON string or a JSON object.
///
/// Kiro's EventStream sometimes sends `input` as a plain string (e.g., `"{\"cmd\":\"ls\"}"`)
/// and sometimes as a JSON object (e.g., `{"cmd": "ls"}`). We normalize both to a String.
fn deserialize_tool_input<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolUsePayload {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tool_use_id: String,
    #[serde(default, deserialize_with = "deserialize_tool_input")]
    input: String,
    #[serde(default)]
    stop: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextUsagePayload {
    #[serde(default)]
    context_usage_percentage: f64,
}

#[derive(serde::Deserialize)]
struct MeteringPayload {
    #[serde(default)]
    usage: f64,
}

/// Payload for messageMetadataEvent containing direct token counts.
///
/// Kiro sends token usage in `tokenUsage` with fields like:
/// `outputTokens`, `totalTokens`, `uncachedInputTokens`, `cacheReadInputTokens`, `cacheWriteInputTokens`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageMetadataPayload {
    #[serde(default)]
    token_usage: Option<TokenUsagePayload>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsagePayload {
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    uncached_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
}

impl Event {
    /// Parse an Event from a decoded Frame.
    pub fn from_frame(frame: &Frame) -> ParseResult<Self> {
        let message_type = frame.message_type().unwrap_or("event");

        match message_type {
            "event" => {
                let event_type = frame.event_type().unwrap_or("");
                match event_type {
                    "assistantResponseEvent" => {
                        let payload: AssistantResponsePayload = frame.payload_as_json()?;
                        Ok(Event::AssistantResponse {
                            content: payload.content,
                        })
                    }
                    "reasoningContentEvent" => {
                        let payload: ReasoningContentPayload = frame.payload_as_json()?;
                        Ok(Event::ReasoningContent {
                            text: payload.text,
                        })
                    }
                    "toolUseEvent" => {
                        let payload: ToolUsePayload = frame.payload_as_json()?;
                        Ok(Event::ToolUse {
                            name: payload.name,
                            tool_use_id: payload.tool_use_id,
                            input: payload.input,
                            stop: payload.stop,
                        })
                    }
                    "contextUsageEvent" => {
                        let payload: ContextUsagePayload = frame.payload_as_json()?;
                        Ok(Event::ContextUsage {
                            percentage: payload.context_usage_percentage,
                        })
                    }
                    "meteringEvent" => {
                        let payload: MeteringPayload = frame.payload_as_json()?;
                        Ok(Event::Metering {
                            usage: payload.usage,
                        })
                    }
                    "messageMetadataEvent" | "metadataEvent" => {
                        let payload: MessageMetadataPayload = frame.payload_as_json()?;
                        if let Some(tu) = payload.token_usage {
                            let input_tokens = if tu.uncached_input_tokens > 0 || tu.cache_read_input_tokens > 0 || tu.cache_write_input_tokens > 0 {
                                tu.uncached_input_tokens + tu.cache_read_input_tokens + tu.cache_write_input_tokens
                            } else if tu.total_tokens > 0 && tu.output_tokens > 0 && tu.total_tokens > tu.output_tokens {
                                tu.total_tokens - tu.output_tokens
                            } else {
                                0
                            };
                            Ok(Event::MessageMetadata {
                                input_tokens,
                                output_tokens: tu.output_tokens,
                                cache_read_tokens: tu.cache_read_input_tokens,
                                cache_write_tokens: tu.cache_write_input_tokens,
                            })
                        } else {
                            Ok(Event::Unknown)
                        }
                    }
                    _ => Ok(Event::Unknown),
                }
            }
            "error" => {
                let code = frame
                    .headers
                    .error_code()
                    .unwrap_or("UnknownError")
                    .to_string();
                let message = frame.payload_as_str();
                Ok(Event::Error { code, message })
            }
            "exception" => {
                let type_name = frame
                    .headers
                    .exception_type()
                    .unwrap_or("UnknownException")
                    .to_string();
                let message = frame.payload_as_str();
                Ok(Event::Exception { type_name, message })
            }
            _ => Err(ParseError::InvalidMessageType(message_type.to_string())),
        }
    }
}

// ---- Text fallback for corrupted binary framing ----

/// Try to parse raw bytes as plain JSON objects when binary EventStream framing
/// is broken (e.g., by an intermediary proxy that strips/corrupts the binary headers).
///
/// Scans the byte stream for JSON objects and attempts to extract Kiro events from them.
pub fn try_parse_text_events(data: &[u8]) -> Vec<Event> {
    let text = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let mut events = Vec::new();
    let mut de = serde_json::Deserializer::from_str(text);

    while let Ok(val) = Value::deserialize(&mut de) {
        if let Some(event) = value_to_event(&val) {
            events.push(event);
        }
    }

    events
}

/// Extract a Kiro Event from a JSON value found in a text fallback scan.
fn value_to_event(val: &Value) -> Option<Event> {
    // Kiro wraps events in { "assistantResponseEvent": { "content": "..." } }
    if let Some(assistant) = val.get("assistantResponseEvent") {
        let content = assistant.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if !content.is_empty() {
            return Some(Event::AssistantResponse {
                content: content.to_string(),
            });
        }
    }

    // { "toolUseEvent": { "toolUseId": "...", "name": "...", "input": "...", "stop": true } }
    if let Some(tool) = val.get("toolUseEvent") {
        let tool_use_id = tool.get("toolUseId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let input = tool.get("input").map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string());
        let stop = tool.get("stop").and_then(|v| v.as_bool()).unwrap_or(false);
        return Some(Event::ToolUse { tool_use_id, name, input, stop });
    }

    // { "contextUsageEvent": { "contextUsagePercentage": 42.5 } }
    if let Some(usage) = val.get("contextUsageEvent") {
        let percentage = usage.get("contextUsagePercentage").and_then(|v| v.as_f64()).unwrap_or(0.0);
        return Some(Event::ContextUsage { percentage });
    }

    // { "messageMetadataEvent": { "tokenUsage": { "outputTokens": ..., ... } } }
    // or { "metadataEvent": { "tokenUsage": { ... } } }
    let metadata = val.get("messageMetadataEvent").or_else(|| val.get("metadataEvent"));
    if let Some(meta) = metadata {
        if let Some(tu) = meta.get("tokenUsage") {
            let output = tu.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let total = tu.get("totalTokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let uncached = tu.get("uncachedInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cache_read = tu.get("cacheReadInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cache_write = tu.get("cacheWriteInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let input = if uncached > 0 || cache_read > 0 || cache_write > 0 {
                uncached + cache_read + cache_write
            } else if total > 0 && output > 0 && total > output {
                total - output
            } else {
                0
            };
            return Some(Event::MessageMetadata {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
            });
        }
    }

    None
}
