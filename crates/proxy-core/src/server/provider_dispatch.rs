//! Provider dispatch trait and implementations.
//!
//! Defines the `ProviderDispatch` trait that encapsulates format-specific
//! request preparation and response handling. This replaces the large
//! `match provider.format` blocks in the handler functions.

use axum::response::Response;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{ModelRoute, ProviderConfig, ProviderFormat};
use crate::convert::anthropic_openai::request::{build_provider_request, prepare_body, prepare_chat_completions_body};
use crate::convert::anthropic_openai::response::{handle_non_stream, handle_non_stream_openai_output};
use crate::convert::anthropic_openai::stream::{handle_stream, handle_stream_openai_output, StreamLogContext};
use crate::convert::passthrough::{handle_non_stream_passthrough, handle_stream_passthrough};
use crate::error::Result;

/// The result of preparing a request body for a specific provider format.
#[derive(Clone)]
pub(crate) struct PreparedRequest {
    /// The transformed JSON body ready to send upstream.
    pub body: Value,
    /// Whether this is a streaming request.
    pub is_stream: bool,
    /// Tool name reverse map (shortened -> original), for response conversion.
    pub tool_name_map: HashMap<String, String>,
    /// The resolved model name (after routing).
    pub provider_model: String,
}

/// Context passed to response handlers, containing timing and logging info.
pub(crate) struct ResponseContext {
    pub request_id: String,
    pub request_start: Instant,
    pub upstream_start: Instant,
    pub upstream_headers_ms: u128,
    pub log_ctx: Option<StreamLogContext>,
}

/// Dyn-compatible trait for provider-format-specific dispatch.
///
/// Each `ProviderFormat` (OpenAI, Anthropic) implements this to handle:
/// - Request body preparation (model routing, format conversion)
/// - HTTP request construction
/// - Response conversion (stream and non-stream)
pub(crate) trait ProviderDispatch: Send + Sync {
    /// Prepare the request body for `/v1/messages` (Anthropic input format).
    #[allow(dead_code)] // Part of the dispatch API; messages path currently uses prepare_body directly.
    fn prepare_messages_body(
        &self,
        body: Value,
        provider: &ProviderConfig,
        global_routes: &[ModelRoute],
    ) -> Result<PreparedRequest>;

    /// Prepare the request body for `/v1/chat/completions` (OpenAI input format).
    fn prepare_completions_body(
        &self,
        body: Value,
        provider: &ProviderConfig,
        global_routes: &[ModelRoute],
    ) -> Result<PreparedRequest>;

    /// Build the HTTP request to send upstream.
    fn build_request(
        &self,
        client: &reqwest::Client,
        provider: &ProviderConfig,
        body: Value,
    ) -> reqwest::RequestBuilder;

    /// Handle a successful upstream response for `/v1/messages`.
    fn handle_messages_response(
        &self,
        upstream_resp: reqwest::Response,
        prepared: PreparedRequest,
        ctx: ResponseContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send + '_>>;

    /// Handle a successful upstream response for `/v1/chat/completions`.
    fn handle_completions_response(
        &self,
        upstream_resp: reqwest::Response,
        prepared: PreparedRequest,
        ctx: ResponseContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send + '_>>;
}

// ---- OpenAI Provider ----

/// Handles providers with `format = "openai"`.
///
/// - `/v1/messages`: converts Anthropic -> OpenAI, response back to Anthropic
/// - `/v1/chat/completions`: passes through directly (already OpenAI format)
pub(crate) struct OpenAiDispatch;

impl ProviderDispatch for OpenAiDispatch {
    fn prepare_messages_body(
        &self,
        body: Value,
        provider: &ProviderConfig,
        global_routes: &[ModelRoute],
    ) -> Result<PreparedRequest> {
        let (body, is_stream, tool_name_map) = prepare_body(body, provider, global_routes)?;
        let provider_model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(provider.model.as_str())
            .to_string();
        Ok(PreparedRequest {
            body,
            is_stream,
            tool_name_map,
            provider_model,
        })
    }

    fn prepare_completions_body(
        &self,
        mut body: Value,
        provider: &ProviderConfig,
        global_routes: &[ModelRoute],
    ) -> Result<PreparedRequest> {
        let requested_model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_stream = body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let provider_model = provider
            .resolve_model_with_routes(Some(&requested_model), global_routes)
            .to_string();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".to_string(), serde_json::json!(provider_model));
        }
        Ok(PreparedRequest {
            body,
            is_stream,
            tool_name_map: HashMap::new(),
            provider_model,
        })
    }

    fn build_request(
        &self,
        client: &reqwest::Client,
        provider: &ProviderConfig,
        body: Value,
    ) -> reqwest::RequestBuilder {
        build_provider_request(client, provider, body)
    }

    fn handle_messages_response(
        &self,
        upstream_resp: reqwest::Response,
        prepared: PreparedRequest,
        ctx: ResponseContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send + '_>> {
        Box::pin(async move {
            if prepared.is_stream {
                handle_stream(
                    upstream_resp,
                    &prepared.provider_model,
                    Arc::new(prepared.tool_name_map),
                    ctx.request_id.clone(),
                    ctx.request_start,
                    ctx.upstream_start,
                    ctx.upstream_headers_ms,
                    ctx.log_ctx,
                )
                .await
            } else {
                handle_non_stream(
                    upstream_resp,
                    &prepared.provider_model,
                    &prepared.tool_name_map,
                    &ctx.request_id,
                    ctx.request_start,
                    ctx.upstream_start,
                    ctx.upstream_headers_ms,
                )
                .await
            }
        })
    }

    fn handle_completions_response(
        &self,
        upstream_resp: reqwest::Response,
        prepared: PreparedRequest,
        ctx: ResponseContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send + '_>> {
        Box::pin(async move {
            if prepared.is_stream {
                handle_stream_passthrough(
                    upstream_resp,
                    ctx.request_id.clone(),
                    ctx.request_start,
                    ctx.upstream_start,
                    ctx.upstream_headers_ms,
                    ctx.log_ctx,
                )
                .await
            } else {
                let body_bytes = upstream_resp.bytes().await?;
                tracing::info!(
                    request_id = ctx.request_id.as_str(),
                    body_bytes = body_bytes.len(),
                    request_total_ms = super::state::elapsed_ms(ctx.request_start),
                    "OpenAI non-stream response complete"
                );
                Ok(axum::response::Response::builder()
                    .status(axum::http::StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body_bytes))
                    .map_err(|e| crate::error::AppError::Request(
                        format!("Failed to build response: {}", e),
                    ))?)
            }
        })
    }
}

// ---- Anthropic Provider ----

/// Handles providers with `format = "anthropic"`.
///
/// - `/v1/messages`: passes through directly (already Anthropic format)
/// - `/v1/chat/completions`: converts OpenAI -> Anthropic, response back to OpenAI
pub(crate) struct AnthropicDispatch;

impl ProviderDispatch for AnthropicDispatch {
    fn prepare_messages_body(
        &self,
        body: Value,
        provider: &ProviderConfig,
        global_routes: &[ModelRoute],
    ) -> Result<PreparedRequest> {
        let (body, is_stream, tool_name_map) = prepare_body(body, provider, global_routes)?;
        let provider_model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(provider.model.as_str())
            .to_string();
        Ok(PreparedRequest {
            body,
            is_stream,
            tool_name_map,
            provider_model,
        })
    }

    fn prepare_completions_body(
        &self,
        body: Value,
        provider: &ProviderConfig,
        global_routes: &[ModelRoute],
    ) -> Result<PreparedRequest> {
        let (body, is_stream, tool_name_map) =
            prepare_chat_completions_body(body, provider, global_routes)?;
        let provider_model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(provider.model.as_str())
            .to_string();
        Ok(PreparedRequest {
            body,
            is_stream,
            tool_name_map,
            provider_model,
        })
    }

    fn build_request(
        &self,
        client: &reqwest::Client,
        provider: &ProviderConfig,
        body: Value,
    ) -> reqwest::RequestBuilder {
        build_provider_request(client, provider, body)
    }

    fn handle_messages_response(
        &self,
        upstream_resp: reqwest::Response,
        prepared: PreparedRequest,
        ctx: ResponseContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send + '_>> {
        Box::pin(async move {
            if prepared.is_stream {
                handle_stream_passthrough(
                    upstream_resp,
                    ctx.request_id.clone(),
                    ctx.request_start,
                    ctx.upstream_start,
                    ctx.upstream_headers_ms,
                    ctx.log_ctx,
                )
                .await
            } else {
                handle_non_stream_passthrough(
                    upstream_resp,
                    &ctx.request_id,
                    ctx.request_start,
                    ctx.upstream_start,
                    ctx.upstream_headers_ms,
                )
                .await
            }
        })
    }

    fn handle_completions_response(
        &self,
        upstream_resp: reqwest::Response,
        prepared: PreparedRequest,
        ctx: ResponseContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send + '_>> {
        Box::pin(async move {
            if prepared.is_stream {
                handle_stream_openai_output(
                    upstream_resp,
                    &prepared.provider_model,
                    ctx.request_id.clone(),
                    ctx.request_start,
                    ctx.upstream_start,
                    ctx.upstream_headers_ms,
                    ctx.log_ctx,
                )
                .await
            } else {
                handle_non_stream_openai_output(
                    upstream_resp,
                    &prepared.provider_model,
                    &ctx.request_id,
                    ctx.request_start,
                    ctx.upstream_start,
                    ctx.upstream_headers_ms,
                )
                .await
            }
        })
    }
}

/// Get the appropriate dispatch implementation for a provider format.
///
/// Returns `None` for Kiro format (which has its own dedicated handler path).
pub(crate) fn get_dispatch(format: &ProviderFormat) -> Option<&'static dyn ProviderDispatch> {
    match format {
        ProviderFormat::Openai => Some(&OpenAiDispatch),
        ProviderFormat::Anthropic => Some(&AnthropicDispatch),
        ProviderFormat::Kiro => None,
    }
}

/// Whether a provider format is handled by its own dedicated handler path
/// rather than the generic [`ProviderDispatch`] flow.
///
/// Centralizes the "is this a self-managed format?" decision so handlers don't
/// each hard-code `provider.format == ProviderFormat::Kiro`. When new
/// self-managed formats are added, only this function needs to change.
pub(crate) fn is_self_managed_format(format: &ProviderFormat) -> bool {
    get_dispatch(format).is_none()
}
