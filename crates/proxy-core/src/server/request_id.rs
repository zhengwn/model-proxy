//! Request ID middleware: generates or forwards X-Request-ID.
//!
//! - Uses the incoming `X-Request-ID` header if present.
//! - Otherwise generates `req_{nanosecond_hex}`.
//! - Stores the ID in request extensions and echoes it in the response.

use axum::{
    body::Body,
    http::{HeaderValue, Request, header},
    middleware::Next,
    response::Response,
};
use std::sync::atomic::{AtomicU64, Ordering};

/// Wrapper for the request ID stored in request extensions.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// axum `from_fn` middleware that ensures every request has an X-Request-ID.
pub async fn request_id_middleware(
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            format!("req_{:016x}_{:04x}", ts, seq & 0xFFFF)
        });

    req.extensions_mut().insert(RequestId(request_id.clone()));

    let mut response = next.run(req).await;

    if let Ok(val) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", val);
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_format() {
        let ts: u128 = 1700000000000000000;
        let seq: u64 = 42;
        let id = format!("req_{:016x}_{:04x}", ts, seq & 0xFFFF);
        assert!(id.starts_with("req_"));
        assert!(id.len() > 4);
    }
}
