//! Kiro (Amazon Q Developer) protocol support.
//!
//! This module handles the full Kiro integration including authentication,
//! account management, protocol conversion, and streaming.
//!
//! ## Module Organization
//!
//! ### Authentication & Account Management
//! - [`auth`] — Token management (KiroAuthManager, KiroCredential)
//! - [`auth_flow`] — OIDC device flow, Social OAuth login
//! - [`account`] — Multi-account manager, load balancing
//!
//! ### Endpoint & Traffic Control
//! - [`endpoint`] — Multi-endpoint URL/header construction
//! - [`endpoint_health`] — Endpoint health scoring & tracking
//! - [`rate_limiter`] — Per-account + global RPM limiting
//! - [`flow_monitor`] — Request/response flow tracking
//!
//! ### Protocol Conversion
//! - [`request`] — Anthropic → Kiro payload conversion
//! - [`responses`] — Kiro → Anthropic non-stream response building
//! - [`stream`] — Kiro EventStream → Anthropic SSE conversion
//! - [`stream_openai`] — Kiro EventStream → OpenAI SSE conversion
//! - [`eventstream`] — AWS EventStream binary frame parser
//!
//! ### Message Processing
//! - [`history`] — Conversation history formatting
//! - [`truncation`] — History auto-truncation (payload size limits)
//! - [`sanitize`] — Conversation sanitizer (format fixes)
//! - [`thinking_parser`] — FSM-based `<thinking>` tag extraction
//!
//! ### Utilities
//! - [`compression`] — Request body gzip compression
//! - [`mcp`] — MCP tool injection (web_search)
//! - [`model_map`] — Model ID normalization & mapping
//! - [`prompt_cache`] — cache_control → cachePoint conversion
//! - [`tool_transform`] — Tool name shortening (>63 chars)

// ---- Authentication & Account Management ----
pub mod account;
pub mod auth;
pub mod auth_flow;

// ---- Endpoint & Traffic Control ----
pub mod endpoint;
pub mod endpoint_health;
pub mod flow_monitor;
pub mod rate_limiter;

// ---- Protocol Conversion ----
pub mod eventstream;
pub mod request;
pub mod responses;
pub mod stream;
pub mod stream_openai;

// ---- Message Processing ----
pub mod history;
pub mod sanitize;
pub mod thinking_parser;
pub mod truncation;

// ---- Utilities ----
pub mod compression;
pub mod mcp;
pub mod model_map;
pub mod prompt_cache;
pub mod tool_transform;
