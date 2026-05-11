use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use futures::StreamExt;
use tracing::{error, info};

use super::state::elapsed_ms;
use crate::error::AppError;

pub(crate) async fn handle_non_stream_passthrough(
    upstream_resp: reqwest::Response,
    request_id: &str,
    request_start: std::time::Instant,
    upstream_start: std::time::Instant,
    upstream_headers_ms: u128,
) -> crate::error::Result<Response> {
    let bytes = upstream_resp.bytes().await?;
    info!(
        request_id,
        body_bytes = bytes.len(),
        upstream_headers_ms,
        upstream_total_ms = elapsed_ms(upstream_start),
        request_total_ms = elapsed_ms(request_start),
        "上游非流式透传响应"
    );
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

pub(crate) async fn handle_stream_passthrough(
    upstream_resp: reqwest::Response,
    request_id: String,
    request_start: std::time::Instant,
    upstream_start: std::time::Instant,
    upstream_headers_ms: u128,
) -> crate::error::Result<Response> {
    let byte_stream = upstream_resp.bytes_stream();
    let stream_start = std::time::Instant::now();
    let mut chunk_count: u64 = 0;
    let body_stream = byte_stream.map(move |result| match result {
        Ok(bytes) => {
            chunk_count += 1;
            Ok(bytes)
        }
        Err(e) => {
            error!(
                request_id = request_id.as_str(),
                error = %e,
                upstream_headers_ms,
                upstream_total_ms = elapsed_ms(upstream_start),
                stream_total_ms = elapsed_ms(stream_start),
                request_total_ms = elapsed_ms(request_start),
                chunks = chunk_count,
                "上游流式透传读取错误"
            );
            Err(std::io::Error::new(std::io::ErrorKind::Other, e))
        }
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body_stream))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}
