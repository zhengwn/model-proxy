//! Parse errors for the AWS EventStream binary protocol.

use std::fmt;

// ---- ParseError ----

#[derive(Debug)]
pub enum ParseError {
    Incomplete { needed: usize, available: usize },
    PreludeCrcMismatch { expected: u32, actual: u32 },
    MessageCrcMismatch { expected: u32, actual: u32 },
    InvalidHeaderType(u8),
    HeaderParseFailed(String),
    MessageTooLarge { length: u32, max: u32 },
    MessageTooSmall { length: u32, min: u32 },
    InvalidMessageType(String),
    PayloadDeserialize(serde_json::Error),
    Io(std::io::Error),
    TooManyErrors { count: usize, last_error: String },
    BufferOverflow { size: usize, max: usize },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete { needed, available } => {
                write!(f, "数据不足: 需要 {} 字节, 可用 {}", needed, available)
            }
            Self::PreludeCrcMismatch { expected, actual } => {
                write!(
                    f,
                    "Prelude CRC 校验失败: 期望 0x{:08x}, 实际 0x{:08x}",
                    expected, actual
                )
            }
            Self::MessageCrcMismatch { expected, actual } => {
                write!(
                    f,
                    "消息 CRC 校验失败: 期望 0x{:08x}, 实际 0x{:08x}",
                    expected, actual
                )
            }
            Self::InvalidHeaderType(t) => write!(f, "无效的 header 类型: {}", t),
            Self::HeaderParseFailed(msg) => write!(f, "Header 解析失败: {}", msg),
            Self::MessageTooLarge { length, max } => {
                write!(f, "消息过大: {} 字节 (最大 {})", length, max)
            }
            Self::MessageTooSmall { length, min } => {
                write!(f, "消息过小: {} 字节 (最小 {})", length, min)
            }
            Self::InvalidMessageType(t) => write!(f, "无效的消息类型: {}", t),
            Self::PayloadDeserialize(e) => write!(f, "Payload JSON 反序列化失败: {}", e),
            Self::Io(e) => write!(f, "IO 错误: {}", e),
            Self::TooManyErrors { count, last_error } => {
                write!(f, "连续 {} 次解析错误, 最后一次: {}", count, last_error)
            }
            Self::BufferOverflow { size, max } => {
                write!(f, "缓冲区溢出: {} 字节 (最大 {})", size, max)
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl From<std::io::Error> for ParseError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for ParseError {
    fn from(e: serde_json::Error) -> Self {
        Self::PayloadDeserialize(e)
    }
}

pub type ParseResult<T> = Result<T, ParseError>;
