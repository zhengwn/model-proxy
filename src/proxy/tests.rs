use super::*;
use crate::config::{Config, ProviderFormat};
use serde_json::json;
use std::collections::HashMap;

fn test_config() -> Config {
    test_config_with_model("deepseek-v4-pro")
}

fn test_config_with_model(model: &str) -> Config {
    Config {
        server: crate::config::ServerConfig {
            port: 4000,
            api_key: None,
            max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
        },
        provider: crate::config::ProviderConfig {
            base_url: "http://127.0.0.1:9999".to_string(),
            api_key: "test-provider-key".to_string(),
            model: model.to_string(),
            format: ProviderFormat::Openai,
            quirks: Default::default(),
        },
    }
}

#[test]
fn tool_choice_uses_sanitized_tool_name() {
    let body = json!({
        "model": "claude-test",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "name": "tool.search",
            "description": "Search",
            "input_schema": {
                "type": "object",
                "properties": {}
            }
        }],
        "tool_choice": {
            "type": "tool",
            "name": "tool.search"
        }
    });

    let (openai, tool_name_map) = convert::anthropic_to_openai(body, &test_config());

    assert_eq!(
        openai["tools"][0]["function"]["name"].as_str(),
        Some("tool_search")
    );
    assert_eq!(
        openai["tool_choice"]["function"]["name"].as_str(),
        Some("tool_search")
    );
    assert_eq!(
        tool_name_map.get("tool_search").map(String::as_str),
        Some("tool.search")
    );
}

#[test]
fn historical_tool_use_uses_sanitized_tool_name() {
    let body = json!({
        "model": "claude-test",
        "messages": [{
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_abc",
                "name": "tool.search",
                "input": {"query": "rust"}
            }]
        }],
        "tools": [{
            "name": "tool.search",
            "description": "Search",
            "input_schema": {
                "type": "object",
                "properties": {}
            }
        }]
    });

    let (openai, _) = convert::anthropic_to_openai(body, &test_config());

    assert_eq!(
        openai["messages"][0]["tool_calls"][0]["function"]["name"].as_str(),
        Some("tool_search")
    );
}

#[test]
fn mixed_tool_result_preserves_user_text() {
    let body = json!({
        "model": "claude-test",
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_abc",
                    "content": "result text"
                },
                {
                    "type": "text",
                    "text": "please continue"
                }
            ]
        }]
    });

    let (openai, _) = convert::anthropic_to_openai(body, &test_config());

    assert_eq!(openai["messages"][0]["role"].as_str(), Some("tool"));
    assert_eq!(
        openai["messages"][0]["tool_call_id"].as_str(),
        Some("call_abc")
    );
    assert_eq!(
        openai["messages"][0]["content"].as_str(),
        Some("result text")
    );
    assert_eq!(openai["messages"][1]["role"].as_str(), Some("user"));
    assert_eq!(
        openai["messages"][1]["content"].as_str(),
        Some("please continue")
    );
}

#[test]
fn max_tokens_uses_provider_model_capabilities() {
    let body = json!({
        "model": "o1-mini",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });

    let (openai, _) = convert::anthropic_to_openai(body, &test_config());

    assert_eq!(openai["max_tokens"].as_u64(), Some(1024));
    assert!(openai.get("max_completion_tokens").is_none());

    let body = json!({
        "model": "claude-test",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });
    let (openai, _) = convert::anthropic_to_openai(body, &test_config_with_model("o1-mini"));

    assert_eq!(openai["max_completion_tokens"].as_u64(), Some(1024));
    assert!(openai.get("max_tokens").is_none());
}

#[test]
fn sse_block_end_supports_lf_and_crlf() {
    assert_eq!(utils::find_sse_block_end("data: {}\n\nrest"), Some((8, 2)));
    assert_eq!(
        utils::find_sse_block_end("data: {}\r\n\r\nrest"),
        Some((8, 4))
    );
}

#[test]
fn stream_message_start_does_not_emit_estimated_input_tokens() {
    let mut stream_id = String::new();
    let mut started = false;
    let mut current_block = None;
    let mut next_content_block_index = 0;
    let mut ended = false;
    let mut pending_message_delta = None;
    let mut output_tokens = 0;
    let mut tool_block_indices = HashMap::new();
    let mut open_tool_blocks = std::collections::BTreeSet::new();
    let mut stop_reason_value = None;
    let tool_name_map = HashMap::new();

    let chunk = json!({
        "id": "chatcmpl_123",
        "choices": [{
            "delta": {"role": "assistant"},
            "finish_reason": null
        }]
    });
    let events = stream::convert_stream_chunk(
        &chunk,
        "deepseek-v4-pro",
        &mut stream_id,
        &mut started,
        &mut current_block,
        &mut next_content_block_index,
        &mut ended,
        &mut pending_message_delta,
        &mut output_tokens,
        &tool_name_map,
        &mut tool_block_indices,
        &mut open_tool_blocks,
        &mut stop_reason_value,
    );
    let text = events.join("");

    assert!(text.contains(r#""input_tokens":0"#));
    assert!(text.contains(r#""output_tokens":0"#));
}

#[test]
fn usage_parts_include_cache_token_variants() {
    let usage = json!({
        "prompt_tokens": 123,
        "completion_tokens": 45,
        "prompt_cache_hit_tokens": 67,
        "prompt_cache_miss_tokens": 89
    });
    let anthropic = stream::build_anthropic_usage(stream::extract_openai_usage_parts(&usage), 0);

    assert_eq!(anthropic["input_tokens"].as_u64(), Some(123));
    assert_eq!(anthropic["output_tokens"].as_u64(), Some(45));
    assert_eq!(anthropic["cache_read_input_tokens"].as_u64(), Some(67));
    assert_eq!(anthropic["cache_creation_input_tokens"].as_u64(), Some(89));

    let usage = json!({
        "input_tokens": 10,
        "output_tokens": 3,
        "prompt_tokens_details": {
            "cached_tokens": 7
        }
    });
    let anthropic = stream::build_anthropic_usage(stream::extract_openai_usage_parts(&usage), 0);

    assert_eq!(anthropic["input_tokens"].as_u64(), Some(10));
    assert_eq!(anthropic["output_tokens"].as_u64(), Some(3));
    assert_eq!(anthropic["cache_read_input_tokens"].as_u64(), Some(7));
}

#[test]
fn stream_tool_use_stop_uses_allocated_block_index() {
    let mut stream_id = String::new();
    let mut started = false;
    let mut current_block = None;
    let mut next_content_block_index = 0;
    let mut ended = false;
    let mut pending_message_delta = None;
    let mut output_tokens = 0;
    let mut tool_block_indices = HashMap::new();
    let mut open_tool_blocks = std::collections::BTreeSet::new();
    let mut stop_reason_value = None;
    let tool_name_map = HashMap::new();

    let text_chunk = json!({
        "id": "chatcmpl_123",
        "choices": [{
            "delta": {
                "role": "assistant",
                "content": "I'll call a tool."
            },
            "finish_reason": null
        }]
    });
    let text_events = stream::convert_stream_chunk(
        &text_chunk,
        "deepseek-v4-pro",
        &mut stream_id,
        &mut started,
        &mut current_block,
        &mut next_content_block_index,
        &mut ended,
        &mut pending_message_delta,
        &mut output_tokens,
        &tool_name_map,
        &mut tool_block_indices,
        &mut open_tool_blocks,
        &mut stop_reason_value,
    );
    let text = text_events.join("");
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains(r#""index":0"#));

    let tool_chunk = json!({
        "id": "chatcmpl_123",
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_abc",
                    "type": "function",
                    "function": {
                        "name": "search",
                        "arguments": "{\"query\":\"rust\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let tool_events = stream::convert_stream_chunk(
        &tool_chunk,
        "deepseek-v4-pro",
        &mut stream_id,
        &mut started,
        &mut current_block,
        &mut next_content_block_index,
        &mut ended,
        &mut pending_message_delta,
        &mut output_tokens,
        &tool_name_map,
        &mut tool_block_indices,
        &mut open_tool_blocks,
        &mut stop_reason_value,
    );
    let text = tool_events.join("");
    assert!(text.contains("event: content_block_stop"));
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains("event: content_block_delta"));
    assert!(text.contains(r#""index":0"#));
    assert!(text.contains(r#""index":1"#));
}

#[test]
fn sse_offset_parser_compacts_and_continues() {
    let mut buffer = String::new();
    let mut read_offset = 0;
    let first_payload = "x".repeat(9000);
    let first_event = format!("data: {}\n\n", first_payload);
    buffer.push_str(&first_event);
    buffer.push_str("data: second\n\n");

    let first = stream::next_sse_block(&buffer, &mut read_offset).unwrap();
    assert_eq!(first, format!("data: {}", first_payload));
    assert_eq!(read_offset, first_event.len());

    stream::compact_sse_buffer(&mut buffer, &mut read_offset);
    assert_eq!(read_offset, 0);
    assert_eq!(buffer, "data: second\n\n");

    let second = stream::next_sse_block(&buffer, &mut read_offset).unwrap();
    assert_eq!(second, "data: second");
}

#[tokio::test]
async fn non_stream_response_restores_original_tool_name() {
    let body = json!({
        "id": "chatcmpl_123",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {
                        "name": "tool_search",
                        "arguments": "{\"query\":\"rust\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 3
        }
    });
    let tool_name_map = HashMap::from([("tool_search".to_string(), "tool.search".to_string())]);

    let anthropic =
        response::convert_non_stream_response(body, "deepseek-v4-pro", &tool_name_map).await;

    assert_eq!(anthropic["stop_reason"].as_str(), Some("tool_use"));
    assert_eq!(
        anthropic["content"][0]["name"].as_str(),
        Some("tool.search")
    );
    assert_eq!(
        anthropic["content"][0]["input"]["query"].as_str(),
        Some("rust")
    );
}
