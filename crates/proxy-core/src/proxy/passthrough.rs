use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{error, info};

use super::state::elapsed_ms;
use super::stream::StreamLogContext;
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
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))
}

pub(crate) async fn handle_stream_passthrough(
    upstream_resp: reqwest::Response,
    request_id: String,
    request_start: std::time::Instant,
    upstream_start: std::time::Instant,
    upstream_headers_ms: u128,
    log_ctx: Option<StreamLogContext>,
) -> crate::error::Result<Response> {
    info!(
        request_id = request_id.as_str(),
        upstream_headers_ms, "开始透传上游流式响应"
    );

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(128);

    tokio::spawn(async move {
        let stream_start = std::time::Instant::now();
        let mut chunk_count: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut byte_stream = upstream_resp.bytes_stream();

        while let Some(result) = byte_stream.next().await {
            match result {
                Ok(bytes) => {
                    chunk_count += 1;
                    total_bytes += bytes.len() as u64;
                    if tx.send(Ok(bytes)).await.is_err() {
                        info!(
                            request_id = request_id.as_str(),
                            chunks = chunk_count,
                            total_bytes,
                            upstream_headers_ms,
                            upstream_total_ms = elapsed_ms(upstream_start),
                            stream_total_ms = elapsed_ms(stream_start),
                            request_total_ms = elapsed_ms(request_start),
                            "流式透传: 客户端断开"
                        );
                        if let Some(log_ctx) = &log_ctx {
                            log_ctx.emit(
                                499,
                                Some(upstream_headers_ms as u64),
                                Some("stream ended: client disconnected".to_string()),
                                None,
                            );
                        }
                        return;
                    }
                }
                Err(e) => {
                    error!(
                        request_id = request_id.as_str(),
                        error = %e,
                        chunks = chunk_count,
                        total_bytes,
                        upstream_headers_ms,
                        upstream_total_ms = elapsed_ms(upstream_start),
                        stream_total_ms = elapsed_ms(stream_start),
                        request_total_ms = elapsed_ms(request_start),
                        "上游流式透传读取错误"
                    );
                    let _ = tx.send(Err(std::io::Error::other(e))).await;
                    if let Some(log_ctx) = &log_ctx {
                        log_ctx.emit(
                            502,
                            Some(upstream_headers_ms as u64),
                            Some("stream ended: upstream error".to_string()),
                            None,
                        );
                    }
                    return;
                }
            }
        }

        info!(
            request_id = request_id.as_str(),
            chunks = chunk_count,
            total_bytes,
            upstream_headers_ms,
            upstream_total_ms = elapsed_ms(upstream_start),
            stream_total_ms = elapsed_ms(stream_start),
            request_total_ms = elapsed_ms(request_start),
            "流式透传响应结束"
        );
        if let Some(log_ctx) = &log_ctx {
            log_ctx.emit(200, Some(upstream_headers_ms as u64), None, None);
        }
    });

    let body_stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(Ok(bytes)) => Some((Ok::<Bytes, std::io::Error>(bytes), rx)),
            Some(Err(e)) => Some((Err(e), rx)),
            None => None,
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body_stream))
        .map_err(|e| AppError::Request(format!("Failed to build response: {}", e)))
}
