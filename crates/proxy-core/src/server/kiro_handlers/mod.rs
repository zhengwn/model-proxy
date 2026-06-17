//! Kiro (Amazon Q Developer) route handlers.
//!
//! Handles all Kiro-specific request processing including:
//! - Auth acquisition (single account / multi-account / tenant)
//! - Multi-endpoint dispatch with retry logic
//! - Streaming and non-streaming response conversion
//! - OpenAI Chat Completions ↔ Kiro conversion
//! - Login / Social OAuth flows
//!
//! Split into focused submodules:
//! - [`dispatch`] — auth acquisition + multi-endpoint dispatch with retry
//! - [`handlers`] — message processing (streaming/non-streaming, Anthropic/OpenAI)
//! - [`login`] — device-flow and social OAuth endpoints

mod dispatch;
mod handlers;
mod login;

pub(crate) use dispatch::{acquire_kiro_auth, dispatch_kiro_request};
pub(crate) use handlers::{
    handle_kiro_chat_completions, handle_kiro_messages, handle_kiro_non_stream,
};
pub use login::{
    proxy_kiro_login_poll, proxy_kiro_login_start, proxy_kiro_social_exchange,
    proxy_kiro_social_start,
};
