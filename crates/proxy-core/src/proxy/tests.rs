use super::*;
use crate::config::{Config, ModelRoute, ProviderFormat};
use serde_json::json;
use std::collections::HashMap;

fn test_config() -> Config {
    test_config_with_model("deepseek-v4-pro")
}

fn test_config_with_model(model: &str) -> Config {
    Config {
        server: crate::config::ServerConfig::default(),
        provider: crate::config::ProviderConfig {
            name: "test".to_string(),
            base_url: "http://127.0.0.1:9999".to_string(),
            api_key: "test-provider-key".to_string(),
            model: model.to_string(),
            format: ProviderFormat::Openai,
            quirks: Default::default(),
            model_routes: Vec::new(),
        },
        active_provider: None,
        providers: Vec::new(),
        model_routes: Vec::new(),
        logging: Default::default(),
        fallback: Default::default(),
    }
}

fn test_config_with_routes() -> Config {
    let mut config = test_config();
    config.model_routes = vec![
        ModelRoute {
            pattern: "sonnet".to_string(),
            target: "deepseek-v4-pro".to_string(),
            reasoning_effort: Some("max".to_string()),
        },
        ModelRoute {
            pattern: "haiku".to_string(),
            target: "deepseek-v4-flash".to_string(),
            reasoning_effort: Some("high".to_string()),
        },
    ];
    config
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

    let config = test_config();
    let (openai, tool_name_map) =
        convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

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
        "messages": [
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": "tool.search",
                    "input": {"query": "rust"}
                }]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_abc",
                    "content": "result"
                }]
            }
        ],
        "tools": [{
            "name": "tool.search",
            "description": "Search",
            "input_schema": {
                "type": "object",
                "properties": {}
            }
        }]
    });

    let config = test_config();
    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(
        openai["messages"][0]["tool_calls"][0]["function"]["name"].as_str(),
        Some("tool_search")
    );
}

#[test]
fn mixed_tool_result_preserves_user_text() {
    let body = json!({
        "model": "claude-test",
        "messages": [
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": "tool.search",
                    "input": {"query": "rust"}
                }]
            },
            {
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
            }
        ],
        "tools": [{
            "name": "tool.search",
            "description": "Search",
            "input_schema": {
                "type": "object",
                "properties": {}
            }
        }]
    });

    let config = test_config();
    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(openai["messages"][0]["role"].as_str(), Some("assistant"));
    assert_eq!(
        openai["messages"][0]["tool_calls"][0]["id"].as_str(),
        Some("call_abc")
    );
    assert_eq!(openai["messages"][1]["role"].as_str(), Some("tool"));
    assert_eq!(
        openai["messages"][1]["tool_call_id"].as_str(),
        Some("call_abc")
    );
    assert_eq!(
        openai["messages"][1]["content"].as_str(),
        Some("result text")
    );
    assert_eq!(openai["messages"][2]["role"].as_str(), Some("user"));
    assert_eq!(
        openai["messages"][2]["content"].as_str(),
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

    let config = test_config();
    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(openai["max_tokens"].as_u64(), Some(1024));
    assert!(openai.get("max_completion_tokens").is_none());

    let body = json!({
        "model": "claude-test",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });
    let config2 = test_config_with_model("o1-mini");
    let (openai, _) = convert::anthropic_to_openai(body, &config2.provider, &config2.model_routes);

    assert_eq!(openai["max_completion_tokens"].as_u64(), Some(1024));
    assert!(openai.get("max_tokens").is_none());
}

#[test]
fn model_routes_map_claude_families_to_provider_models() {
    let config = test_config_with_routes();

    let body = json!({
        "model": "claude-sonnet-4-7",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);
    assert_eq!(openai["model"].as_str(), Some("deepseek-v4-pro"));

    let body = json!({
        "model": "claude-haiku-4-5-20251001",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);
    assert_eq!(openai["model"].as_str(), Some("deepseek-v4-flash"));
}

#[test]
fn model_routes_fallback_to_default_provider_model() {
    let config = test_config_with_routes();
    let body = json!({
        "model": "claude-opus-4-1",
        "messages": [{"role": "user", "content": "hi"}]
    });

    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(openai["model"].as_str(), Some("deepseek-v4-pro"));
}

#[test]
fn model_capabilities_use_routed_provider_model() {
    let mut config = test_config();
    config.model_routes = vec![ModelRoute {
        pattern: "sonnet".to_string(),
        target: "o1-mini".to_string(),
        reasoning_effort: None,
    }];
    let body = json!({
        "model": "claude-sonnet-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });

    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(openai["model"].as_str(), Some("o1-mini"));
    assert_eq!(openai["max_completion_tokens"].as_u64(), Some(1024));
    assert!(openai.get("max_tokens").is_none());
}

#[test]
fn provider_quirk_forces_route_specific_reasoning_effort_to_deepseek() {
    let mut config = test_config_with_routes();
    config.provider.quirks.supports_reasoning_effort = true;
    let body = json!({
        "model": "claude-sonnet-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "enabled", "budget_tokens": 1024}
    });

    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(openai["model"].as_str(), Some("deepseek-v4-pro"));
    assert_eq!(openai["reasoning_effort"].as_str(), Some("max"));

    let body = json!({
        "model": "claude-haiku-4-5-20251001",
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "adaptive"}
    });
    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(openai["model"].as_str(), Some("deepseek-v4-flash"));
    assert_eq!(openai["reasoning_effort"].as_str(), Some("high"));
}

#[test]
fn json_schema_output_config_uses_openai_response_format_shape() {
    let config = test_config_with_model("gpt-4o");
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {
            "format": {
                "type": "json_schema",
                "name": "tool_result",
                "schema": {
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" }
                    },
                    "required": ["ok"]
                }
            }
        }
    });

    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(
        openai["response_format"]["type"].as_str(),
        Some("json_schema")
    );
    assert!(openai["response_format"].get("schema").is_none());
    assert_eq!(
        openai["response_format"]["json_schema"]["name"].as_str(),
        Some("tool_result")
    );
    assert_eq!(
        openai["response_format"]["json_schema"]["schema"]["required"][0].as_str(),
        Some("ok")
    );
}

#[test]
fn no_json_schema_quirk_downgrades_to_json_object() {
    let mut config = test_config_with_model("gpt-4o");
    config.provider.quirks.no_json_schema = true;
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {
            "format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" }
                    }
                }
            }
        }
    });

    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(
        openai["response_format"]["type"].as_str(),
        Some("json_object")
    );
    assert!(openai["response_format"].get("json_schema").is_none());
    assert_eq!(openai["messages"][0]["role"].as_str(), Some("system"));
    assert!(openai["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .contains("JSON Schema"));
}

#[test]
fn deepseek_models_use_schema_prompt_instead_of_response_format() {
    let config = test_config_with_model("deepseek-v4-pro");
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {
            "format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" }
                    }
                }
            }
        }
    });

    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert!(openai.get("response_format").is_none());
    assert_eq!(openai["messages"][0]["role"].as_str(), Some("system"));
    assert!(openai["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .contains("JSON Schema"));
}

#[test]
fn incomplete_assistant_tool_calls_are_removed_from_history() {
    let config = test_config();
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "toolu_missing",
                        "name": "tool.search",
                        "input": {"query": "rust"}
                    }
                ]
            },
            {
                "role": "user",
                "content": "continue"
            }
        ],
        "tools": [{
            "name": "tool.search",
            "description": "Search",
            "input_schema": {"type": "object", "properties": {}}
        }]
    });

    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert!(openai["messages"][0].get("tool_calls").is_none());
    assert_eq!(openai["messages"][0]["content"].as_str(), Some(""));
    assert_eq!(openai["messages"][1]["role"].as_str(), Some("user"));
}

#[test]
fn partial_assistant_tool_calls_keep_only_answered_calls() {
    let config = test_config();
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "toolu_answered",
                        "name": "tool.search",
                        "input": {"query": "rust"}
                    },
                    {
                        "type": "tool_use",
                        "id": "toolu_missing",
                        "name": "tool.lookup",
                        "input": {"id": 1}
                    }
                ]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_answered",
                    "content": "result"
                }]
            }
        ],
        "tools": [
            {
                "name": "tool.search",
                "description": "Search",
                "input_schema": {"type": "object", "properties": {}}
            },
            {
                "name": "tool.lookup",
                "description": "Lookup",
                "input_schema": {"type": "object", "properties": {}}
            }
        ]
    });

    let (openai, _) = convert::anthropic_to_openai(body, &config.provider, &config.model_routes);

    assert_eq!(
        openai["messages"][0]["tool_calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        openai["messages"][0]["tool_calls"][0]["id"].as_str(),
        Some("call_answered")
    );
    assert_eq!(openai["messages"][1]["role"].as_str(), Some("tool"));
    assert_eq!(
        openai["messages"][1]["tool_call_id"].as_str(),
        Some("call_answered")
    );
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
    let mut state = stream::StreamConversionState::new();
    let tool_name_map = HashMap::new();

    let chunk = json!({
        "id": "chatcmpl_123",
        "choices": [{
            "delta": {"role": "assistant"},
            "finish_reason": null
        }]
    });
    let events =
        stream::convert_stream_chunk(&chunk, "deepseek-v4-pro", &mut state, &tool_name_map);
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
    let mut state = stream::StreamConversionState::new();
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
    let text_events =
        stream::convert_stream_chunk(&text_chunk, "deepseek-v4-pro", &mut state, &tool_name_map);
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
    let tool_events =
        stream::convert_stream_chunk(&tool_chunk, "deepseek-v4-pro", &mut state, &tool_name_map);
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

    let anthropic = response::convert_non_stream_response(body, "deepseek-v4-pro", &tool_name_map);

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

// ---- openai_to_anthropic tests ----

fn anthropic_provider_config() -> crate::config::ProviderConfig {
    crate::config::ProviderConfig {
        name: "anthropic-test".to_string(),
        base_url: "http://127.0.0.1:9999".to_string(),
        api_key: "test-key".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        format: ProviderFormat::Anthropic,
        quirks: Default::default(),
        model_routes: Vec::new(),
    }
}

#[test]
fn openai_to_anthropic_basic_user_message() {
    let body = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    assert_eq!(result["messages"][0]["role"].as_str(), Some("user"));
    assert_eq!(result["messages"][0]["content"].as_str(), Some("Hello"));
    assert_eq!(result["model"].as_str(), Some("claude-sonnet-4-20250514"));
}

#[test]
fn openai_to_anthropic_system_messages() {
    let body = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "system", "content": "Be concise."},
            {"role": "user", "content": "Hi"}
        ]
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    // Multiple system messages become array in `system` field
    let system = result.get("system").unwrap();
    assert!(system.is_array());
    assert_eq!(system.as_array().unwrap().len(), 2);
    assert_eq!(system[0]["text"].as_str(), Some("You are helpful."));
    assert_eq!(system[1]["text"].as_str(), Some("Be concise."));
    // Only user message in messages array
    assert_eq!(result["messages"].as_array().unwrap().len(), 1);
    assert_eq!(result["messages"][0]["role"].as_str(), Some("user"));
}

#[test]
fn openai_to_anthropic_single_system_message_as_string() {
    let body = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hi"}
        ]
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    // Single system message becomes a string
    assert_eq!(result["system"].as_str(), Some("You are helpful."));
}

#[test]
fn openai_to_anthropic_assistant_tool_calls() {
    let body = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "user", "content": "Search for Rust"},
            {
                "role": "assistant",
                "content": "I'll search for that.",
                "tool_calls": [{
                    "id": "call_abc123",
                    "type": "function",
                    "function": {
                        "name": "search",
                        "arguments": "{\"query\": \"Rust\"}"
                    }
                }]
            }
        ]
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    let assistant_msg = &result["messages"][1];
    assert_eq!(assistant_msg["role"].as_str(), Some("assistant"));
    let content = assistant_msg["content"].as_array().unwrap();
    // Should have text block + tool_use block
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"].as_str(), Some("text"));
    assert_eq!(content[0]["text"].as_str(), Some("I'll search for that."));
    assert_eq!(content[1]["type"].as_str(), Some("tool_use"));
    assert_eq!(content[1]["id"].as_str(), Some("toolu_abc123"));
    assert_eq!(content[1]["name"].as_str(), Some("search"));
    assert_eq!(content[1]["input"]["query"].as_str(), Some("Rust"));
}

#[test]
fn openai_to_anthropic_tool_result_messages() {
    let body = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "user", "content": "Search for Rust"},
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_abc123",
                    "type": "function",
                    "function": {"name": "search", "arguments": "{\"query\":\"Rust\"}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call_abc123", "content": "Rust is a language."}
        ]
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    let tool_msg = &result["messages"][2];
    assert_eq!(tool_msg["role"].as_str(), Some("user"));
    let content = tool_msg["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"].as_str(), Some("tool_result"));
    assert_eq!(content[0]["tool_use_id"].as_str(), Some("toolu_abc123"));
    assert_eq!(content[0]["content"].as_str(), Some("Rust is a language."));
}

#[test]
fn openai_to_anthropic_consecutive_tool_results_merged() {
    let body = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "user", "content": "Do two things"},
            {
                "role": "assistant",
                "tool_calls": [
                    {"id": "call_a1", "type": "function", "function": {"name": "search", "arguments": "{}"}},
                    {"id": "call_a2", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
                ]
            },
            {"role": "tool", "tool_call_id": "call_a1", "content": "result1"},
            {"role": "tool", "tool_call_id": "call_a2", "content": "result2"}
        ]
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    // Consecutive tool messages should be merged into one user message
    let tool_msg = &result["messages"][2];
    assert_eq!(tool_msg["role"].as_str(), Some("user"));
    let content = tool_msg["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["tool_use_id"].as_str(), Some("toolu_a1"));
    assert_eq!(content[0]["content"].as_str(), Some("result1"));
    assert_eq!(content[1]["tool_use_id"].as_str(), Some("toolu_a2"));
    assert_eq!(content[1]["content"].as_str(), Some("result2"));
}

#[test]
fn openai_to_anthropic_tool_definitions() {
    let body = json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "Hi"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search the web",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"}
                    }
                }
            }
        }]
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    let tools = result["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"].as_str(), Some("tool"));
    assert_eq!(tools[0]["name"].as_str(), Some("search"));
    assert_eq!(tools[0]["description"].as_str(), Some("Search the web"));
    assert_eq!(
        tools[0]["input_schema"]["properties"]["query"]["type"].as_str(),
        Some("string")
    );
}

#[test]
fn openai_to_anthropic_max_tokens() {
    let body = json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "Hi"}],
        "max_tokens": 1000,
        "max_completion_tokens": 2000
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    // max_completion_tokens takes priority over max_tokens
    assert_eq!(result["max_tokens"].as_u64(), Some(2000));
}

#[test]
fn openai_to_anthropic_stop_and_parameters() {
    let body = json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "Hi"}],
        "stop": ["END", "STOP"],
        "temperature": 0.7,
        "top_p": 0.9
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    assert_eq!(result["stop_sequences"][0].as_str(), Some("END"));
    assert_eq!(result["stop_sequences"][1].as_str(), Some("STOP"));
    assert_eq!(result["temperature"].as_f64(), Some(0.7));
    assert_eq!(result["top_p"].as_f64(), Some(0.9));
}

#[test]
fn openai_to_anthropic_tool_choice_required() {
    let body = json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "Hi"}],
        "tool_choice": "required"
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    assert_eq!(result["tool_choice"]["type"].as_str(), Some("any"));
}

#[test]
fn openai_to_anthropic_tool_choice_named() {
    let body = json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "Hi"}],
        "tool_choice": {
            "type": "function",
            "function": {"name": "search"}
        }
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    assert_eq!(result["tool_choice"]["type"].as_str(), Some("tool"));
    assert_eq!(result["tool_choice"]["name"].as_str(), Some("search"));
}

#[test]
fn openai_to_anthropic_response_format_json_schema() {
    let body = json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "Hi"}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "result",
                "schema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}}
                }
            }
        }
    });
    let provider = anthropic_provider_config();
    let result = convert::openai_to_anthropic(body, &provider, &[]);

    assert_eq!(
        result["output_config"]["format"]["type"].as_str(),
        Some("json_schema")
    );
    assert_eq!(
        result["output_config"]["format"]["name"].as_str(),
        Some("result")
    );
}

#[test]
fn openai_to_anthropic_model_routing() {
    let body = json!({
        "model": "claude-sonnet-4-7",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let mut provider = anthropic_provider_config();
    provider.model = "default-model".to_string();
    let routes = vec![ModelRoute {
        pattern: "sonnet".to_string(),
        target: "routed-model".to_string(),
        reasoning_effort: None,
    }];
    let result = convert::openai_to_anthropic(body, &provider, &routes);

    assert_eq!(result["model"].as_str(), Some("routed-model"));
}

// ---- convert_anthropic_to_openai_response tests ----

#[test]
fn anthropic_to_openai_response_basic_text() {
    let body = json!({
        "id": "msg_abc123",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "Hello world"}],
        "model": "claude-sonnet-4-20250514",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let result = response::convert_anthropic_to_openai_response(body, "test-model");

    assert!(result["id"].as_str().unwrap().starts_with("chatcmpl-"));
    assert_eq!(result["object"].as_str(), Some("chat.completion"));
    assert_eq!(result["model"].as_str(), Some("test-model"));
    let choice = &result["choices"][0];
    assert_eq!(choice["message"]["content"].as_str(), Some("Hello world"));
    assert_eq!(choice["finish_reason"].as_str(), Some("stop"));
    assert_eq!(result["usage"]["prompt_tokens"].as_u64(), Some(10));
    assert_eq!(result["usage"]["completion_tokens"].as_u64(), Some(5));
    assert_eq!(result["usage"]["total_tokens"].as_u64(), Some(15));
}

#[test]
fn anthropic_to_openai_response_tool_use() {
    let body = json!({
        "id": "msg_abc",
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "tool_use",
            "id": "toolu_abc123",
            "name": "search",
            "input": {"query": "Rust"}
        }],
        "model": "test",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let result = response::convert_anthropic_to_openai_response(body, "test-model");

    let choice = &result["choices"][0];
    assert_eq!(choice["finish_reason"].as_str(), Some("tool_calls"));
    assert!(choice["message"]["content"].is_null());
    let tool_calls = choice["message"]["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["id"].as_str(), Some("call_abc123"));
    assert_eq!(tool_calls[0]["function"]["name"].as_str(), Some("search"));
    let args: serde_json::Value =
        serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["query"].as_str(), Some("Rust"));
}

#[test]
fn anthropic_to_openai_response_thinking() {
    let body = json!({
        "id": "msg_abc",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "Let me think...", "signature": ""},
            {"type": "text", "text": "The answer is 42."}
        ],
        "model": "test",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    });
    let result = response::convert_anthropic_to_openai_response(body, "test-model");

    let message = &result["choices"][0]["message"];
    assert_eq!(
        message["reasoning_content"].as_str(),
        Some("Let me think...")
    );
    assert_eq!(message["content"].as_str(), Some("The answer is 42."));
}

#[test]
fn anthropic_to_openai_response_stop_reasons() {
    let test_cases = vec![
        ("end_turn", "stop"),
        ("max_tokens", "length"),
        ("tool_use", "tool_calls"),
        ("unknown_reason", "stop"),
    ];

    for (anthropic_sr, expected_openai_fr) in test_cases {
        let body = json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "model": "test",
            "stop_reason": anthropic_sr,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let result = response::convert_anthropic_to_openai_response(body, "test");
        assert_eq!(
            result["choices"][0]["finish_reason"].as_str(),
            Some(expected_openai_fr),
            "stop_reason '{}' should map to '{}'",
            anthropic_sr,
            expected_openai_fr
        );
    }
}

// ---- convert_anthropic_stream_chunk tests ----

#[test]
fn anthropic_stream_chunk_text_delta() {
    let mut state = stream::OpenAiStreamOutputState::new();
    state.started = true;
    state.stream_id = "chatcmpl-test123".to_string();

    let data = json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": "Hello"}
    });
    let events = stream::convert_anthropic_stream_chunk("content_block_delta", &data, "test-model", &mut state);

    assert_eq!(events.len(), 1);
    assert!(events[0].contains("\"content\":\"Hello\""));
    assert!(events[0].starts_with("data: "));
}

#[test]
fn anthropic_stream_chunk_thinking_delta() {
    let mut state = stream::OpenAiStreamOutputState::new();
    state.started = true;
    state.stream_id = "chatcmpl-test".to_string();

    let data = json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "thinking_delta", "thinking": "reasoning..." }
    });
    let events = stream::convert_anthropic_stream_chunk("content_block_delta", &data, "test-model", &mut state);

    assert_eq!(events.len(), 1);
    assert!(events[0].contains("\"reasoning_content\":\"reasoning...\""));
}

#[test]
fn anthropic_stream_chunk_tool_use_start() {
    let mut state = stream::OpenAiStreamOutputState::new();
    state.started = true;
    state.stream_id = "chatcmpl-test".to_string();

    let data = json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {
            "type": "tool_use",
            "id": "toolu_abc123",
            "name": "search",
            "input": {}
        }
    });
    let events = stream::convert_anthropic_stream_chunk("content_block_start", &data, "test-model", &mut state);

    assert_eq!(events.len(), 1);
    assert!(events[0].contains("\"call_abc123\""));
    assert!(events[0].contains("\"search\""));
    assert_eq!(state.tool_call_counter, 1);
}

#[test]
fn anthropic_stream_chunk_message_delta_stop() {
    let mut state = stream::OpenAiStreamOutputState::new();
    state.started = true;
    state.stream_id = "chatcmpl-test".to_string();

    let data = json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
        "usage": {"output_tokens": 42}
    });
    let events = stream::convert_anthropic_stream_chunk("message_delta", &data, "test-model", &mut state);

    assert_eq!(events.len(), 1);
    assert!(events[0].contains("\"finish_reason\":\"stop\""));
    assert!(events[0].contains("\"completion_tokens\":42"));
    assert!(state.ended);
}

#[test]
fn anthropic_stream_chunk_message_start() {
    let mut state = stream::OpenAiStreamOutputState::new();

    let data = json!({
        "type": "message_start",
        "message": {
            "id": "msg_abc",
            "type": "message",
            "role": "assistant",
            "model": "test",
            "usage": {"input_tokens": 10, "output_tokens": 0}
        }
    });
    let events = stream::convert_anthropic_stream_chunk("message_start", &data, "test-model", &mut state);

    assert_eq!(events.len(), 1);
    assert!(state.started);
    assert_eq!(state.stream_id, "chatcmpl-msg_abc");
    assert!(events[0].contains("\"role\":\"assistant\""));
    assert_eq!(state.input_tokens, 10);
}
