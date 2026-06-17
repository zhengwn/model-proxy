//! Anthropic ↔ OpenAI request body conversion and upstream request construction.
//!
//! Split into focused submodules:
//! - [`reasoning`] — model capability heuristics, reasoning-effort / JSON-schema helpers
//! - [`content`] — content-block, tool, tool-choice conversion and id mapping
//! - [`to_openai`] — Anthropic → OpenAI request conversion
//! - [`to_anthropic`] — OpenAI → Anthropic request conversion
//! - [`url`] — upstream endpoint URL construction
//!
//! All items keep their original `convert::anthropic_openai::request::*` paths via re-exports.

use serde_json::{json, Value};
use std::collections::HashMap;

use crate::config::ProviderFormat;
use crate::error::Result;

mod content;
mod reasoning;
mod to_anthropic;
mod to_openai;
mod url;

// ---- Re-exports (preserve original paths) ----

// Used by sibling modules (stream, response) and convert::mod.rs.
pub(crate) use content::{anthropic_id_to_openai, openai_id_to_anthropic};
pub use content::clean_schema;
pub(crate) use to_anthropic::openai_to_anthropic;
pub(crate) use to_openai::anthropic_to_openai;
pub use url::{anthropic_messages_url, openai_chat_completions_url};

/// Prepare body for `/v1/chat/completions` endpoint (OpenAI-format input).
/// Mirrors `prepare_body()` but accepts OpenAI Chat Completions format.
pub(crate) fn prepare_chat_completions_body(
    mut body: Value,
    provider: &crate::config::ProviderConfig,
    global_routes: &[crate::config::ModelRoute],
) -> Result<(Value, bool, HashMap<String, String>)> {
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match provider.format {
        ProviderFormat::Openai => {
            // Already in OpenAI format — just resolve model name
            if let Some(obj) = body.as_object_mut() {
                let provider_model = provider
                    .resolve_model_with_routes(
                        obj.get("model").and_then(|v| v.as_str()),
                        global_routes,
                    )
                    .to_string();
                obj.insert("model".to_string(), json!(provider_model));
            }
            Ok((body, stream, HashMap::new()))
        }
        ProviderFormat::Anthropic => {
            let anthropic_body = openai_to_anthropic(body, provider, global_routes);
            Ok((anthropic_body, stream, HashMap::new()))
        }
        ProviderFormat::Kiro => {
            // Kiro 转换由 convert/kiro/request.rs 处理
            Err(crate::error::AppError::Request(
                "Kiro format should use prepare_kiro_body() instead".to_string(),
            ))
        }
    }
}

pub(crate) fn prepare_body(
    mut body: Value,
    provider: &crate::config::ProviderConfig,
    global_routes: &[crate::config::ModelRoute],
) -> Result<(Value, bool, HashMap<String, String>)> {
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match provider.format {
        ProviderFormat::Openai => {
            let (openai_body, tool_name_map) = anthropic_to_openai(body, provider, global_routes);
            Ok((openai_body, stream, tool_name_map))
        }
        ProviderFormat::Anthropic => {
            if let Some(obj) = body.as_object_mut() {
                let provider_model = provider
                    .resolve_model_with_routes(
                        obj.get("model").and_then(|v| v.as_str()),
                        global_routes,
                    )
                    .to_string();
                obj.insert("model".to_string(), json!(provider_model));
            }
            Ok((body, stream, HashMap::new()))
        }
        ProviderFormat::Kiro => {
            // Kiro 转换由 convert/kiro/request.rs 处理，此路径不应到达
            Err(crate::error::AppError::Request(
                "Kiro format should use prepare_kiro_body() instead of prepare_body()".to_string(),
            ))
        }
    }
}

pub(crate) fn build_provider_request(
    client: &reqwest::Client,
    provider: &crate::config::ProviderConfig,
    body: Value,
) -> reqwest::RequestBuilder {
    match provider.format {
        ProviderFormat::Openai => {
            let url = openai_chat_completions_url(&provider.base_url);
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", provider.api_key))
                .json(&body)
        }
        ProviderFormat::Anthropic => {
            let url = anthropic_messages_url(&provider.base_url);
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-api-key", provider.api_key.as_str())
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        }
        ProviderFormat::Kiro => {
            // Kiro 请求构建由 convert/kiro 模块处理，此为占位
            // 实际的 Kiro 请求需要特殊 headers（x-amzn-kiro-agent-mode 等）
            let url = format!(
                "https://q.{region}.amazonaws.com/generateAssistantResponse",
                region = provider
                    .kiro_config
                    .as_ref()
                    .and_then(|k| k.api_region.as_deref())
                    .or(provider.kiro_config.as_ref().map(|k| k.region.as_str()))
                    .unwrap_or("us-east-1")
            );
            client
                .post(&url)
                .header("content-type", "application/json")
                .json(&body)
        }
    }
}
