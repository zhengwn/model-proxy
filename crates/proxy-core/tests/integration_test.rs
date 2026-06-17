//! Integration tests for proxy-core.
//!
//! These tests start a real HTTP server and send requests to verify
//! end-to-end behavior including routing, auth, concurrency limits, and fallback.

use axum::{routing::post, Json, Router};
use proxy_core::config::{
    Config, FallbackConfig, ModelRoute, ProviderConfig, ProviderFormat, ProviderQuirks,
    ServerConfig,
};
use proxy_core::logging::LogConfig;
use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Start a mock upstream server that returns a fixed OpenAI-format response.
async fn start_mock_openai_server(response: Value) -> (SocketAddr, CancellationToken) {
    let token = CancellationToken::new();
    let cancel = token.clone();

    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let resp = response.clone();
            async move { Json(resp) }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .unwrap();
    });

    (addr, token)
}

/// Start a mock upstream that returns a specific status code.
async fn start_mock_error_server(status: u16) -> (SocketAddr, CancellationToken) {
    let token = CancellationToken::new();
    let cancel = token.clone();

    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            (
                axum::http::StatusCode::from_u16(status).unwrap(),
                Json(json!({"error": {"message": "mock error", "type": "test"}})),
            )
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .unwrap();
    });

    (addr, token)
}

fn make_config(port: u16, provider_url: &str) -> Config {
    Config {
        server: ServerConfig {
            port,
            api_key: Some("test-key".to_string()),
            ..Default::default()
        },
        provider: ProviderConfig::placeholder(),
        active_provider: Some("test".to_string()),
        providers: vec![ProviderConfig {
            name: "test".to_string(),
            base_url: provider_url.to_string(),
            api_key: "upstream-key".to_string(),
            model: "test-model".to_string(),
            format: ProviderFormat::Openai,
            quirks: ProviderQuirks::default(),
            model_routes: Vec::new(),
            kiro_config: None,
        }],
        model_routes: Vec::new(),
        model_routes_enabled: true,
        logging: LogConfig::default(),
        fallback: FallbackConfig::default(),
    }
}

fn find_available_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let port = find_available_port();
    let (mock_addr, mock_token) = start_mock_openai_server(json!({})).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());

    // Wait for server to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{}/health", port))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn auth_rejects_missing_key() {
    let port = find_available_port();
    let (mock_addr, mock_token) = start_mock_openai_server(json!({})).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .body(r#"{"model":"test","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn auth_accepts_valid_x_api_key() {
    let port = find_available_port();
    let mock_response = json!({
        "id": "chatcmpl_test",
        "choices": [{"message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1}
    });
    let (mock_addr, mock_token) = start_mock_openai_server(mock_response).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .body(r#"{"model":"test","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn auth_accepts_bearer_token() {
    let port = find_available_port();
    let mock_response = json!({
        "id": "chatcmpl_test",
        "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1}
    });
    let (mock_addr, mock_token) = start_mock_openai_server(mock_response).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-key")
        .body(r#"{"model":"test","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn proxy_converts_anthropic_to_openai_and_back() {
    let port = find_available_port();
    let mock_response = json!({
        "id": "chatcmpl_abc",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Hello from the proxy!"
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    });
    let (mock_addr, mock_token) = start_mock_openai_server(mock_response).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    // Verify Anthropic format response
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "Hello from the proxy!");
    assert_eq!(body["usage"]["input_tokens"], 10);
    assert_eq!(body["usage"]["output_tokens"], 5);

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn model_routing_applies_to_requests() {
    let port = find_available_port();
    let mock_response = json!({
        "id": "chatcmpl_route",
        "choices": [{"message": {"role": "assistant", "content": "routed"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1}
    });
    let (mock_addr, mock_token) = start_mock_openai_server(mock_response).await;
    let mut config = make_config(port, &format!("http://{}", mock_addr));
    config.model_routes = vec![ModelRoute {
        pattern: "sonnet".to_string(),
        target: "custom-model".to_string(),
        reasoning_effort: None,
    }];

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // The response model should reflect the routed model
    assert_eq!(body["model"], "test-model");

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn concurrency_limit_rejects_excess_requests() {
    let port = find_available_port();

    // Use a slow mock server that takes 500ms to respond
    let slow_token = CancellationToken::new();
    let slow_cancel = slow_token.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Json(json!({
                "id": "chatcmpl_slow",
                "choices": [{"message": {"role": "assistant", "content": "slow"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { slow_cancel.cancelled().await })
            .await
            .unwrap();
    });

    let mut config = make_config(port, &format!("http://{}", mock_addr));
    config.server.max_concurrent_requests = 1; // Only allow 1 concurrent request

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let request_body = json!({
        "model": "test",
        "messages": [{"role": "user", "content": "hi"}]
    });

    // Send two concurrent requests
    let req1 = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&request_body)
        .send();

    let req2 = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&request_body)
        .send();

    // Small delay to ensure req1 gets the permit first
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (resp1, resp2) = tokio::join!(req1, req2);
    let resp1 = resp1.unwrap();
    let resp2 = resp2.unwrap();

    // One should succeed (200) and one should be rejected (429)
    let statuses = vec![resp1.status().as_u16(), resp2.status().as_u16()];
    assert!(
        statuses.contains(&200) && statuses.contains(&429),
        "Expected one 200 and one 429, got {:?}",
        statuses
    );

    token.cancel();
    let _ = handle.await;
    slow_token.cancel();
}

#[tokio::test]
async fn chat_completions_endpoint_forwards_openai_format() {
    let port = find_available_port();
    let mock_response = json!({
        "id": "chatcmpl_oai",
        "choices": [{"message": {"role": "assistant", "content": "openai response"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3}
    });
    let (mock_addr, mock_token) = start_mock_openai_server(mock_response.clone()).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // Response should be in OpenAI format (passthrough)
    assert_eq!(body["choices"][0]["message"]["content"], "openai response");

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn openai_provider_base_url_with_v1_does_not_duplicate_path() {
    let port = find_available_port();
    let mock_response = json!({
        "id": "chatcmpl_v1_base",
        "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3}
    });
    let (mock_addr, mock_token) = start_mock_openai_server(mock_response).await;
    let config = make_config(port, &format!("http://{}/v1", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "ok");

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn fallback_tries_next_provider_on_error() {
    let port = find_available_port();

    // First provider returns 500
    let (error_addr, error_token) = start_mock_error_server(500).await;

    // Second provider returns success
    let mock_response = json!({
        "id": "chatcmpl_fallback",
        "choices": [{"message": {"role": "assistant", "content": "from fallback"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2}
    });
    let (success_addr, success_token) = start_mock_openai_server(mock_response).await;

    let config = Config {
        server: ServerConfig {
            port,
            api_key: Some("test-key".to_string()),
            ..Default::default()
        },
        provider: ProviderConfig::placeholder(),
        active_provider: Some("failing".to_string()),
        providers: vec![
            ProviderConfig {
                name: "failing".to_string(),
                base_url: format!("http://{}", error_addr),
                api_key: "key1".to_string(),
                model: "model1".to_string(),
                format: ProviderFormat::Openai,
                quirks: ProviderQuirks::default(),
                model_routes: Vec::new(),
                kiro_config: None,
            },
            ProviderConfig {
                name: "backup".to_string(),
                base_url: format!("http://{}", success_addr),
                api_key: "key2".to_string(),
                model: "model2".to_string(),
                format: ProviderFormat::Openai,
                quirks: ProviderQuirks::default(),
                model_routes: Vec::new(),
                kiro_config: None,
            },
        ],
        model_routes: vec![ModelRoute {
            pattern: "test".to_string(),
            target: "routed-fallback-model".to_string(),
            reasoning_effort: None,
        }],
        model_routes_enabled: true,
        logging: LogConfig::default(),
        fallback: FallbackConfig {
            enabled: true,
            on_status_codes: vec![500, 502, 503],
            max_attempts: 3,
        },
    };

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    // Should succeed via fallback
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "from fallback");
    assert_eq!(body["model"], "routed-fallback-model");

    token.cancel();
    let _ = handle.await;
    error_token.cancel();
    success_token.cancel();
}

#[tokio::test]
async fn fallback_disabled_returns_original_error() {
    let port = find_available_port();
    let (error_addr, error_token) = start_mock_error_server(500).await;

    let mut config = make_config(port, &format!("http://{}", error_addr));
    config.fallback.enabled = false; // Fallback disabled

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    // Should return the upstream error directly
    assert_eq!(resp.status(), 500);

    token.cancel();
    let _ = handle.await;
    error_token.cancel();
}

#[tokio::test]
async fn upstream_error_returns_proper_error_format() {
    let port = find_available_port();
    let (error_addr, error_token) = start_mock_error_server(429).await;

    let mut config = make_config(port, &format!("http://{}", error_addr));
    config.fallback.enabled = false;

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "upstream_error");

    token.cancel();
    let _ = handle.await;
    error_token.cancel();
}

/// Start a mock upstream server that returns a streaming SSE response.
async fn start_mock_streaming_server(chunks: Vec<String>) -> (SocketAddr, CancellationToken) {
    let token = CancellationToken::new();
    let cancel = token.clone();

    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let chunks = chunks.clone();
            async move {
                let body = chunks.join("");
                (
                    axum::http::StatusCode::OK,
                    [
                        (axum::http::header::CONTENT_TYPE, "text/event-stream"),
                        (axum::http::header::CACHE_CONTROL, "no-cache"),
                    ],
                    body,
                )
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .unwrap();
    });

    (addr, token)
}

#[tokio::test]
async fn streaming_response_converts_openai_to_anthropic_sse() {
    let port = find_available_port();

    // Simulate a multi-chunk OpenAI streaming response
    let sse_chunks = vec![
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ];

    let (mock_addr, mock_token) = start_mock_streaming_server(sse_chunks).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "test",
            "max_tokens": 1024,
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );

    let body = resp.text().await.unwrap();

    // Verify the SSE stream contains expected Anthropic events
    assert!(
        body.contains("event: message_start"),
        "missing message_start"
    );
    assert!(
        body.contains("event: content_block_start"),
        "missing content_block_start"
    );
    assert!(
        body.contains("event: content_block_delta"),
        "missing content_block_delta"
    );
    assert!(
        body.contains("event: content_block_stop"),
        "missing content_block_stop"
    );
    assert!(
        body.contains("event: message_delta"),
        "missing message_delta"
    );
    assert!(body.contains("event: message_stop"), "missing message_stop");

    // Verify content deltas contain the text
    assert!(body.contains("Hello"), "missing 'Hello' in stream");
    assert!(body.contains(" world"), "missing ' world' in stream");

    // Verify message_start has correct structure
    assert!(
        body.contains(r#""role":"assistant"#),
        "missing role in message_start"
    );
    assert!(
        body.contains(r#""type":"message"#),
        "missing type in message_start"
    );

    // Verify stop reason
    assert!(
        body.contains(r#""stop_reason":"end_turn"#),
        "missing stop_reason"
    );

    // Verify usage is present in message_delta
    assert!(
        body.contains(r#""input_tokens":5"#),
        "missing input_tokens in usage"
    );
    assert!(
        body.contains(r#""output_tokens":2"#),
        "missing output_tokens in usage"
    );

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn streaming_response_with_tool_calls() {
    let port = find_available_port();

    // Simulate a streaming response with tool calls
    let sse_chunks = vec![
        "data: {\"id\":\"chatcmpl_tools\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_tools\",\"choices\":[{\"delta\":{\"content\":\"Let me search.\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_tools\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\"\"}}]},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_tools\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"rust\\\"}\"}}]},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_tools\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ];

    let (mock_addr, mock_token) = start_mock_streaming_server(sse_chunks).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "test",
            "stream": true,
            "messages": [{"role": "user", "content": "search for rust"}],
            "tools": [{"name": "search", "description": "Search", "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();

    // Verify tool_use block appears
    assert!(
        body.contains(r#""type":"tool_use"#),
        "missing tool_use block"
    );
    assert!(body.contains(r#""name":"search"#), "missing tool name");
    assert!(
        body.contains("input_json_delta"),
        "missing input_json_delta"
    );

    // Verify stop reason is tool_use
    assert!(
        body.contains(r#""stop_reason":"tool_use"#),
        "missing tool_use stop_reason"
    );

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}

#[tokio::test]
async fn streaming_response_with_thinking() {
    let port = find_available_port();

    // Simulate a streaming response with reasoning_content (thinking)
    let sse_chunks = vec![
        "data: {\"id\":\"chatcmpl_think\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_think\",\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me think...\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_think\",\"choices\":[{\"delta\":{\"content\":\"The answer is 42.\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl_think\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":10}}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ];

    let (mock_addr, mock_token) = start_mock_streaming_server(sse_chunks).await;
    let config = make_config(port, &format!("http://{}", mock_addr));

    let token = CancellationToken::new();
    let handle = proxy_core::start_server(config, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", port))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .json(&json!({
            "model": "test",
            "stream": true,
            "messages": [{"role": "user", "content": "What is the meaning of life?"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();

    // Verify thinking block appears
    assert!(
        body.contains(r#""type":"thinking"#),
        "missing thinking block"
    );
    assert!(body.contains("thinking_delta"), "missing thinking_delta");
    assert!(body.contains("Let me think..."), "missing thinking content");

    // Verify text block appears after thinking
    assert!(body.contains(r#""type":"text"#), "missing text block");
    assert!(body.contains("text_delta"), "missing text_delta");
    assert!(body.contains("The answer is 42."), "missing text content");

    // Should have at least 2 content_block_start (thinking + text)
    let block_starts = body.matches("content_block_start").count();
    assert!(
        block_starts >= 2,
        "expected at least 2 content_block_start, got {}",
        block_starts
    );

    token.cancel();
    let _ = handle.await;
    mock_token.cancel();
}
