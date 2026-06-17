//! Model listing, token counting, and OpenAI Responses API handlers.

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::now_epoch_secs;
use crate::server::auth::{check_auth, check_auth_tenant, AuthResult};
use crate::server::kiro_handlers::{acquire_kiro_auth, dispatch_kiro_request, handle_kiro_non_stream};
use crate::error::{AppError, Result};

/// Handle `GET /v1/models` — returns available models in OpenAI format.
pub async fn proxy_models(
    State(state): State<crate::server::state::AppState>,
) -> Result<Response> {
    const MODEL_CACHE_TTL_SECS: u64 = 3600; // 1 hour

    let provider = state.current_provider();

    // Check cache first
    if let Some(cache_arc) = state.model_cache() {
        let cache = cache_arc.lock().await;
        if cache.0.elapsed().as_secs() < MODEL_CACHE_TTL_SECS {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(cache.1.to_string()))
                .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)));
        }
    }

    // Build model list
    let models: Vec<Value> = if provider.format == crate::config::ProviderFormat::Kiro {
        // Try to fetch from Kiro API
        let region = provider
            .kiro_config
            .as_ref()
            .and_then(|k| k.api_region.as_deref().or(Some(k.region.as_str())))
            .unwrap_or("us-east-1");
        let list_url = format!("https://runtime.{}.kiro.dev/ListAvailableModels", region);

        // Get auth token
        let token = if let Some(auth_arc) = state.kiro_auth() {
            let mut auth = auth_arc.lock().await;
            auth.get_valid_token().await.unwrap_or_default()
        } else {
            String::new()
        };

        // Try API call
        let api_models = if !token.is_empty() {
            match state
                .client
                .get(&list_url)
                .header("Authorization", format!("Bearer {}", token))
                .header("x-amz-target", "AmazonCodeWhispererStreamingService.ListAvailableModels")
                .header("Content-Type", "application/x-amz-json-1.0")
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    resp.json::<Value>().await.ok()
                }
                _ => None,
            }
        } else {
            None
        };

        // Parse API response or fall back to static list
        if let Some(api_resp) = api_models {
            // Kiro API returns array of model objects with modelId field
            if let Some(arr) = api_resp.as_array() {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("modelId").or_else(|| m.get("id"))?.as_str()?;
                        Some(json!({
                            "id": id,
                            "object": "model",
                            "created": now_epoch_secs(),
                            "owned_by": "kiro"
                        }))
                    })
                    .collect()
            } else {
                static_kiro_model_list()
            }
        } else {
            static_kiro_model_list()
        }
    } else {
        // Non-Kiro provider: return configured model + model routes
        let mut models = vec![json!({
            "id": provider.model,
            "object": "model",
            "created": now_epoch_secs(),
            "owned_by": provider.name
        })];
        for route in provider.model_routes.iter() {
            models.push(json!({
                "id": route.target,
                "object": "model",
                "created": now_epoch_secs(),
                "owned_by": provider.name
            }));
        }
        models
    };

    let response = json!({
        "object": "list",
        "data": models
    });

    // Update cache
    if let Some(cache_arc) = state.model_cache() {
        let mut cache = cache_arc.lock().await;
        *cache = (Instant::now(), response.clone());
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))
}

/// Static fallback model list from KIRO_MODELS constant.
fn static_kiro_model_list() -> Vec<Value> {
    use crate::convert::kiro::model_map::KIRO_MODELS;
    KIRO_MODELS
        .iter()
        .map(|(name, _)| {
            json!({
                "id": name,
                "object": "model",
                "created": 0u64,
                "owned_by": "kiro"
            })
        })
        .collect()
}

/// Handle `POST /v1/messages/count_tokens` — estimates token count for an Anthropic request.
/// Used by Claude Code to decide conversation compaction timing.
pub async fn proxy_count_tokens(
    State(state): State<crate::server::state::AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response> {
    // Validate auth
    check_auth(&headers, &state.config)?;

    let _provider = state.current_provider();

    // Estimate tokens from the request body
    let mut total_tokens: u64 = 0;

    // System prompt tokens
    if let Some(system) = body.get("system") {
        let text = match system {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        total_tokens += crate::convert::kiro::stream::estimate_tokens(&text) as u64;
    }

    // Message tokens
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            // Role overhead
            total_tokens += 4;
            if let Some(content) = msg.get("content") {
                match content {
                    Value::String(s) => {
                        total_tokens += crate::convert::kiro::stream::estimate_tokens(s) as u64;
                    }
                    Value::Array(arr) => {
                        for block in arr {
                            match block.get("type").and_then(|v| v.as_str()) {
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                        total_tokens +=
                                            crate::convert::kiro::stream::estimate_tokens(t) as u64;
                                    }
                                }
                                Some("image") => total_tokens += 100,
                                Some("tool_use") => {
                                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                    let input = block.get("input").map(|v| v.to_string()).unwrap_or_default();
                                    total_tokens += crate::convert::kiro::stream::estimate_tokens(&format!("{} {}", name, input)) as u64;
                                }
                                Some("tool_result") => {
                                    let result_text = match block.get("content") {
                                        Some(Value::String(s)) => s.clone(),
                                        Some(Value::Array(arr)) => arr
                                            .iter()
                                            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                                            .collect::<Vec<_>>()
                                            .join("\n"),
                                        _ => String::new(),
                                    };
                                    total_tokens += crate::convert::kiro::stream::estimate_tokens(&result_text) as u64;
                                }
                                Some("thinking") => {
                                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                                        total_tokens += crate::convert::kiro::stream::estimate_tokens(t) as u64;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Tool definition tokens
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        for tool in tools {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
            total_tokens += crate::convert::kiro::stream::estimate_tokens(&format!("{} {}", name, desc)) as u64;
            // Schema tokens
            if let Some(schema) = tool.get("input_schema") {
                total_tokens += crate::convert::kiro::stream::estimate_tokens(&schema.to_string()) as u64;
            }
        }
    }

    let response = json!({
        "input_tokens": total_tokens
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))
}

/// Handle `POST /v1/responses` — OpenAI Responses API (used by Codex CLI).
/// Converts Responses API format to Kiro format via Anthropic intermediate.
pub async fn proxy_responses(
    State(state): State<crate::server::state::AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response> {
    use crate::convert::kiro::responses::responses_to_kiro;
    use crate::convert::kiro::stream::handle_stream_anthropic_output as kiro_handle_stream;
    use std::collections::HashMap;

    check_auth(&headers, &state.config)?;

    let tenant_refresh_token = match check_auth_tenant(&headers, &state.config) {
        AuthResult::TenantOk { refresh_token } => Some(refresh_token),
        _ => None,
    };

    let provider = state.current_provider();
    let model_routes = state.current_model_routes();
    let global_routes = if state.is_model_routes_enabled() { model_routes.as_slice() } else { &[] };

    if provider.format != crate::config::ProviderFormat::Kiro {
        return Err(AppError::Request(
            "/v1/responses 仅支持 Kiro provider".to_string(),
        ));
    }

    let kiro_config = provider.kiro_config.as_ref().ok_or_else(|| {
        AppError::Request("Kiro provider 缺少 kiro_config 配置".to_string())
    })?;

    let (mut kiro_payload, _tool_name_map) = responses_to_kiro(&body, &provider, global_routes)?;
    let auth_info = acquire_kiro_auth(&state, kiro_config, tenant_refresh_token.as_deref()).await?;

    // Inject profileArn into payload if available
    if let Some(ref arn) = auth_info.profile_arn {
        if let Some(obj) = kiro_payload.as_object_mut() {
            obj.insert("profileArn".to_string(), json!(arn));
        }
    }

    let payload_bytes = serde_json::to_vec(&kiro_payload)?;
    let is_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let dispatch_result = match dispatch_kiro_request(
        &state,
        &kiro_payload,
        &payload_bytes,
        &auth_info,
        kiro_config,
        "responses",
        is_stream,
    )
    .await
    {
        Ok(r) => r,
        Err(AppError::UpstreamStatus(status, body)) => {
            return Ok((
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({"error": {"message": body, "type": "upstream_error"}})),
            ).into_response());
        }
        Err(e) => return Err(e),
    };

    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("kiro");

    if is_stream {
        kiro_handle_stream(
            dispatch_result.response,
            model,
            HashMap::new(),
            "responses".to_string(),
            Instant::now(),
            Instant::now(),
            dispatch_result.upstream_headers_ms,
            None,
            kiro_config.thinking_mode.as_deref(),
            std::time::Duration::from_secs(kiro_config.first_token_timeout.unwrap_or(15)),
            std::time::Duration::from_secs(kiro_config.streaming_read_timeout.unwrap_or(300)),
            state.kiro.as_ref().map(|k| k.truncation_state.clone()),
        )
        .await
    } else {
        let tool_map = HashMap::new();
        handle_kiro_non_stream(
            dispatch_result.response,
            model,
            &tool_map,
            "responses",
            Instant::now(),
            Instant::now(),
            dispatch_result.upstream_headers_ms,
        )
        .await
    }
}
