use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use tracing::error;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    #[allow(dead_code)]
    Config(String),
    Request(String),
    Http(reqwest::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
    Unauthorized,
    PayloadTooLarge,
    UpstreamInvalidResponse(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Config(msg) => write!(f, "配置错误: {}", msg),
            AppError::Request(msg) => write!(f, "请求错误: {}", msg),
            AppError::Http(e) => write!(f, "HTTP 错误: {}", e),
            AppError::Json(e) => write!(f, "JSON 错误: {}", e),
            AppError::Io(e) => write!(f, "IO 错误: {}", e),
            AppError::Unauthorized => write!(f, "未授权"),
            AppError::PayloadTooLarge => write!(f, "请求体过大"),
            AppError::UpstreamInvalidResponse(msg) => write!(f, "上游响应格式异常: {}", msg),
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::Config(msg) => {
                error!("{}", self);
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
            AppError::Request(msg) => {
                error!("{}", self);
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            AppError::Http(e) => {
                error!("HTTP 错误: {}", e);
                (StatusCode::BAD_GATEWAY, format!("上游服务错误: {}", e))
            }
            AppError::Json(e) => {
                error!("JSON 解析错误: {}", e);
                (StatusCode::BAD_REQUEST, format!("JSON 解析错误: {}", e))
            }
            AppError::Io(e) => {
                error!("IO 错误: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("内部错误: {}", e),
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
            AppError::UpstreamInvalidResponse(msg) => {
                error!("{}", self);
                (StatusCode::BAD_GATEWAY, msg.clone())
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
