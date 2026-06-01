use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use tracing::{error, warn};

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    Config(String),
    Request(String),
    Http(reqwest::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
    Unauthorized,
    PayloadTooLarge,
    TooManyRequests,
    UpstreamInvalidResponse(String),
    /// Upstream returned a non-success HTTP status with a body.
    UpstreamStatus(u16, String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Config(msg) => write!(f, "config error: {}", msg),
            AppError::Request(msg) => write!(f, "request error: {}", msg),
            AppError::Http(e) => write!(f, "HTTP error: {}", e),
            AppError::Json(e) => write!(f, "JSON error: {}", e),
            AppError::Io(e) => write!(f, "IO error: {}", e),
            AppError::Unauthorized => write!(f, "unauthorized"),
            AppError::PayloadTooLarge => write!(f, "payload too large"),
            AppError::TooManyRequests => write!(f, "too many concurrent requests"),
            AppError::UpstreamInvalidResponse(msg) => {
                write!(f, "upstream invalid response: {}", msg)
            }
            AppError::UpstreamStatus(code, msg) => {
                write!(f, "upstream error {}: {}", code, msg)
            }
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::Config(msg) => {
                error!("配置错误: {}", msg);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Configuration error: {}", msg),
                )
            }
            AppError::Request(msg) => {
                error!("请求错误: {}", msg);
                (StatusCode::BAD_REQUEST, format!("Bad request: {}", msg))
            }
            AppError::Http(e) => {
                error!("HTTP 错误: {}", e);
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream service error: {}", e),
                )
            }
            AppError::Json(e) => {
                error!("JSON 解析错误: {}", e);
                (StatusCode::BAD_REQUEST, format!("JSON parse error: {}", e))
            }
            AppError::Io(e) => {
                error!("IO 错误: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Internal error: {}", e),
                )
            }
            AppError::Unauthorized => {
                error!("认证失败");
                (StatusCode::UNAUTHORIZED, "Unauthorized".to_string())
            }
            AppError::PayloadTooLarge => {
                error!("请求体超限");
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Payload too large".to_string(),
                )
            }
            AppError::TooManyRequests => {
                warn!("并发请求数超限");
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many concurrent requests, please retry later".to_string(),
                )
            }
            AppError::UpstreamInvalidResponse(msg) => {
                error!("上游响应异常: {}", msg);
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream invalid response: {}", msg),
                )
            }
            AppError::UpstreamStatus(code, msg) => {
                error!("上游返回错误 {}: {}", code, msg);
                let status = StatusCode::from_u16(*code).unwrap_or(StatusCode::BAD_GATEWAY);
                (status, msg.clone())
            }
        };

        let body = Json(json!({
            "type": "error",
            "error": {
                "type": "proxy_error",
                "message": message
            }
        }));

        (status, body).into_response()
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        AppError::Http(err)
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        AppError::Json(err)
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::Io(err)
    }
}
