//! Error classification for Kiro multi-account failover decisions.

/// Error classification for failover decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorClass {
    /// Account-specific error, try next account
    Recoverable,
    /// Account suspended, should be auto-disabled
    Suspended,
    /// Request-level error, return to client immediately
    Fatal,
}

/// Classify an HTTP status code for failover decisions.
pub fn classify_error(status: u16, body: &str) -> ErrorClass {
    let lower_body = body.to_lowercase();
    match status {
        402 | 403 | 429 => {
            if lower_body.contains("temporarily_suspended") || lower_body.contains("suspended") {
                ErrorClass::Suspended
            } else {
                // 402 quota exceeded, 403 token expired/invalid, 429 rate limit
                ErrorClass::Recoverable
            }
        }
        400 => {
            if body.contains("INVALID_MODEL_ID") {
                ErrorClass::Recoverable // Model not on this tier
            } else {
                // CONTENT_LENGTH_EXCEEDS or other malformed request
                ErrorClass::Fatal
            }
        }
        422 => ErrorClass::Fatal, // Validation error
        _ if status >= 500 => ErrorClass::Fatal, // Server error
        _ => ErrorClass::Fatal,
    }
}
