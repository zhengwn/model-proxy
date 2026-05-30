//! Web Search MCP tool emulation for Kiro.
//!
//! Provides web search capability by:
//! 1. Auto-injecting a `web_search` tool definition into requests
//! 2. Intercepting `web_search` tool calls during streaming
//! 3. Calling the Kiro MCP API to execute searches
//! 4. Returning search results wrapped in `<web_search>` XML tags

use serde_json::{json, Value};
use tracing::{info, warn};

/// Web search tool definition injected into Kiro requests.
pub fn web_search_tool_definition() -> Value {
    json!({
        "toolSpecification": {
            "name": "web_search",
            "description": "Search the web for current information. Use this when you need up-to-date facts, documentation, or real-time data.",
            "inputSchema": {
                "json": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query"
                        }
                    },
                    "required": ["query"]
                }
            }
        }
    })
}

/// Execute a web search via the Kiro MCP API.
/// Returns the search results as a formatted string.
pub async fn execute_web_search(
    client: &reqwest::Client,
    api_host: &str,
    token: &str,
    query: &str,
) -> Result<String, String> {
    let mcp_url = format!("{}/mcp", api_host);

    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "web_search",
            "arguments": {
                "query": query
            }
        }
    });

    let resp = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", token))
        .header("x-amz-target", "AmazonCodeWhispererStreamingService.InvokeMCP")
        .timeout(std::time::Duration::from_secs(60))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("MCP 请求失败: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("MCP API 返回 {}: {}", status, body));
    }

    let resp_json: Value = resp
        .json()
        .await
        .map_err(|e| format!("MCP 响应解析失败: {}", e))?;

    // MCP response: result.content[0].text is a JSON string
    let text = resp_json
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");

    if text.is_empty() {
        return Ok("No search results found.".to_string());
    }

    // Parse the inner JSON string
    let search_results: Value = serde_json::from_str(text).unwrap_or(Value::String(text.to_string()));

    Ok(format_search_results(&search_results))
}

/// Format search results as XML tags for the model to consume.
fn format_search_results(results: &Value) -> String {
    let mut output = String::from("<web_search>\n");

    if let Some(items) = results.as_array() {
        for (i, item) in items.iter().enumerate() {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = item.get("snippet").or_else(|| item.get("description"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            output.push_str(&format!(
                "<result index=\"{}\">\n<title>{}</title>\n<url>{}</url>\n<snippet>{}</snippet>\n</result>\n",
                i + 1, title, url, snippet
            ));
        }
    } else if let Some(text) = results.as_str() {
        output.push_str(text);
        output.push('\n');
    }

    output.push_str("</web_search>");
    output
}

/// Generate an Anthropic SSE response for web search results.
/// Used when intercepting web_search tool calls during streaming.
pub fn generate_anthropic_search_sse(
    stream_id: &str,
    _tool_use_id: &str,
    search_results: &str,
    output_tokens: usize,
) -> Vec<String> {
    let mut events = Vec::new();

    // message_start
    events.push(format!(
        "event: message_start\ndata: {}\n\n",
        json!({
            "type": "message_start",
            "message": {
                "id": stream_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": "kiro",
                "stop_reason": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        })
    ));

    // text content block
    events.push(format!(
        "event: content_block_start\ndata: {}\n\n",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        })
    ));

    events.push(format!(
        "event: content_block_delta\ndata: {}\n\n",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": format!("Search results:\n{}", search_results)}
        })
    ));

    events.push(format!(
        "event: content_block_stop\ndata: {}\n\n",
        json!({"type": "content_block_stop", "index": 0})
    ));

    // message_delta + message_stop
    events.push(format!(
        "event: message_delta\ndata: {}\n\n",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": output_tokens}
        })
    ));

    events.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string());

    events
}

/// Generate an OpenAI SSE response for web search results.
pub fn generate_openai_search_sse(
    chat_id: &str,
    search_results: &str,
) -> Vec<String> {
    let mut events = Vec::new();

    // Role chunk
    events.push(format!(
        "data: {}\n\n",
        json!({
            "id": chat_id,
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
        })
    ));

    // Content chunk
    events.push(format!(
        "data: {}\n\n",
        json!({
            "id": chat_id,
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"content": format!("Search results:\n{}", search_results)}, "finish_reason": null}]
        })
    ));

    // Finish chunk
    events.push(format!(
        "data: {}\n\n",
        json!({
            "id": chat_id,
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
        })
    ));

    events.push("data: [DONE]\n\n".to_string());

    events
}

/// Check if a tool call in the stream is a web_search call.
pub fn is_web_search_call(tool_name: &str) -> bool {
    tool_name == "web_search"
}

/// Extract the search query from a web_search tool call input.
pub fn extract_search_query(input: &Value) -> Option<String> {
    input
        .get("query")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_search_tool_definition_format() {
        let tool = web_search_tool_definition();
        assert_eq!(tool["toolSpecification"]["name"], "web_search");
        assert!(tool["toolSpecification"]["description"].as_str().unwrap().contains("Search"));
        let schema = &tool["toolSpecification"]["inputSchema"]["json"];
        assert_eq!(schema["type"], "object");
        assert!(schema["required"].as_array().unwrap().contains(&json!("query")));
    }

    #[test]
    fn format_search_results_xml() {
        let results = json!([
            {"title": "Test Title", "url": "https://example.com", "snippet": "Test snippet"}
        ]);
        let formatted = format_search_results(&results);
        assert!(formatted.contains("<web_search>"));
        assert!(formatted.contains("<title>Test Title</title>"));
        assert!(formatted.contains("<url>https://example.com</url>"));
        assert!(formatted.contains("</web_search>"));
    }

    #[test]
    fn is_web_search_call_check() {
        assert!(is_web_search_call("web_search"));
        assert!(!is_web_search_call("other_tool"));
    }

    #[test]
    fn extract_search_query_test() {
        let input = json!({"query": "rust programming"});
        assert_eq!(extract_search_query(&input), Some("rust programming".to_string()));

        let empty = json!({});
        assert_eq!(extract_search_query(&empty), None);
    }

    #[test]
    fn anthropic_search_sse_events() {
        let events = generate_anthropic_search_sse("msg_123", "toolu_456", "test results", 100);
        assert!(events.len() >= 5);
        assert!(events[0].contains("message_start"));
        assert!(events.last().unwrap().contains("message_stop"));
    }

    #[test]
    fn openai_search_sse_events() {
        let events = generate_openai_search_sse("chatcmpl_123", "test results");
        assert!(events.len() >= 3);
        assert!(events.last().unwrap().contains("[DONE]"));
    }
}
