//! AWS EventStream binary protocol parser for Kiro API responses.
//!
//! Implements the binary framing protocol used by AWS services for streaming responses.
//! Each frame consists of:
//! - Prelude (12 bytes): total_length, header_length, prelude_crc
//! - Headers (variable): binary-encoded key-value pairs
//! - Payload (variable): JSON body
//! - Message CRC (4 bytes): CRC32 of everything except itself

use bytes::{Buf, BytesMut};
use crc::{Crc, CRC_32_ISO_HDLC};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use tracing::{error, warn};

mod error;
mod event;

pub use error::{ParseError, ParseResult};
pub use event::{try_parse_text_events, Event};

// ---- CRC32 ----

const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

pub fn crc32(data: &[u8]) -> u32 {
    CRC32.checksum(data)
}

// ---- Constants ----

const PRELUDE_SIZE: usize = 12;
const MIN_MESSAGE_SIZE: usize = PRELUDE_SIZE + 4; // 16
const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024; // 16 MB


// ---- HeaderValue ----

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderValueType {
    BoolTrue = 0,
    BoolFalse = 1,
    Byte = 2,
    Short = 3,
    Integer = 4,
    Long = 5,
    ByteArray = 6,
    String = 7,
    Timestamp = 8,
    Uuid = 9,
}

impl TryFrom<u8> for HeaderValueType {
    type Error = ParseError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::BoolTrue),
            1 => Ok(Self::BoolFalse),
            2 => Ok(Self::Byte),
            3 => Ok(Self::Short),
            4 => Ok(Self::Integer),
            5 => Ok(Self::Long),
            6 => Ok(Self::ByteArray),
            7 => Ok(Self::String),
            8 => Ok(Self::Timestamp),
            9 => Ok(Self::Uuid),
            _ => Err(ParseError::InvalidHeaderType(value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Integer(i32),
    Long(i64),
    ByteArray(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

impl HeaderValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }
}

// ---- Headers ----

#[derive(Debug, Clone, Default)]
pub struct Headers {
    inner: HashMap<String, HeaderValue>,
}

impl Headers {
    fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    fn insert(&mut self, name: String, value: HeaderValue) {
        self.inner.insert(name, value);
    }

    pub fn get(&self, name: &str) -> Option<&HeaderValue> {
        self.inner.get(name)
    }

    pub fn get_string(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| v.as_str())
    }

    pub fn message_type(&self) -> Option<&str> {
        self.get_string(":message-type")
    }

    pub fn event_type(&self) -> Option<&str> {
        self.get_string(":event-type")
    }

    pub fn exception_type(&self) -> Option<&str> {
        self.get_string(":exception-type")
    }

    pub fn error_code(&self) -> Option<&str> {
        self.get_string(":error-code")
    }
}

// ---- Frame ----

#[derive(Debug, Clone)]
pub struct Frame {
    pub headers: Headers,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn message_type(&self) -> Option<&str> {
        self.headers.message_type()
    }

    pub fn event_type(&self) -> Option<&str> {
        self.headers.event_type()
    }

    pub fn payload_as_json<T: DeserializeOwned>(&self) -> ParseResult<T> {
        serde_json::from_slice(&self.payload).map_err(ParseError::PayloadDeserialize)
    }

    pub fn payload_as_str(&self) -> String {
        String::from_utf8_lossy(&self.payload).to_string()
    }
}

// ---- Header parsing ----

fn ensure_bytes(data: &[u8], needed: usize) -> ParseResult<()> {
    if data.len() < needed {
        Err(ParseError::Incomplete {
            needed,
            available: data.len(),
        })
    } else {
        Ok(())
    }
}

fn parse_header_value(
    data: &[u8],
    value_type: HeaderValueType,
    global_offset: &mut usize,
) -> ParseResult<HeaderValue> {
    let mut local_offset = 0usize;
    let value = match value_type {
        HeaderValueType::BoolTrue => HeaderValue::Bool(true),
        HeaderValueType::BoolFalse => HeaderValue::Bool(false),
        HeaderValueType::Byte => {
            ensure_bytes(data, 1)?;
            local_offset = 1;
            HeaderValue::Byte(data[0] as i8)
        }
        HeaderValueType::Short => {
            ensure_bytes(data, 2)?;
            local_offset = 2;
            HeaderValue::Short(i16::from_be_bytes([data[0], data[1]]))
        }
        HeaderValueType::Integer => {
            ensure_bytes(data, 4)?;
            local_offset = 4;
            HeaderValue::Integer(i32::from_be_bytes([data[0], data[1], data[2], data[3]]))
        }
        HeaderValueType::Long | HeaderValueType::Timestamp => {
            ensure_bytes(data, 8)?;
            local_offset = 8;
            let val = i64::from_be_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]);
            if value_type == HeaderValueType::Timestamp {
                HeaderValue::Timestamp(val)
            } else {
                HeaderValue::Long(val)
            }
        }
        HeaderValueType::ByteArray => {
            ensure_bytes(data, 2)?;
            let len = u16::from_be_bytes([data[0], data[1]]) as usize;
            ensure_bytes(&data[2..], len)?;
            local_offset = 2 + len;
            HeaderValue::ByteArray(data[2..2 + len].to_vec())
        }
        HeaderValueType::String => {
            ensure_bytes(data, 2)?;
            let len = u16::from_be_bytes([data[0], data[1]]) as usize;
            ensure_bytes(&data[2..], len)?;
            local_offset = 2 + len;
            HeaderValue::String(String::from_utf8_lossy(&data[2..2 + len]).to_string())
        }
        HeaderValueType::Uuid => {
            ensure_bytes(data, 16)?;
            local_offset = 16;
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&data[..16]);
            HeaderValue::Uuid(uuid)
        }
    };
    *global_offset += local_offset;
    Ok(value)
}

fn parse_headers(data: &[u8], header_length: usize) -> ParseResult<Headers> {
    if data.len() < header_length {
        return Err(ParseError::Incomplete {
            needed: header_length,
            available: data.len(),
        });
    }
    let mut headers = Headers::new();
    let mut offset = 0usize;

    while offset < header_length {
        if offset >= data.len() {
            break;
        }
        let name_len = data[offset] as usize;
        offset += 1;
        if name_len == 0 {
            return Err(ParseError::HeaderParseFailed(
                "header name length cannot be 0".to_string(),
            ));
        }
        if offset + name_len > data.len() {
            return Err(ParseError::Incomplete {
                needed: name_len,
                available: data.len() - offset,
            });
        }
        let name = String::from_utf8_lossy(&data[offset..offset + name_len]).to_string();
        offset += name_len;

        if offset >= data.len() {
            return Err(ParseError::Incomplete {
                needed: 1,
                available: 0,
            });
        }
        let value_type = HeaderValueType::try_from(data[offset])?;
        offset += 1;

        let value = parse_header_value(&data[offset..], value_type, &mut offset)?;
        headers.insert(name, value);
    }

    Ok(headers)
}

// ---- Frame parsing ----

/// Parse a single frame from the buffer.
/// Returns `Ok(None)` if not enough data, `Ok(Some((frame, consumed)))` on success.
pub fn parse_frame(buffer: &[u8]) -> ParseResult<Option<(Frame, usize)>> {
    if buffer.len() < PRELUDE_SIZE {
        return Ok(None);
    }

    let total_length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let header_length = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
    let prelude_crc = u32::from_be_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);

    if total_length < MIN_MESSAGE_SIZE as u32 {
        return Err(ParseError::MessageTooSmall {
            length: total_length,
            min: MIN_MESSAGE_SIZE as u32,
        });
    }
    if total_length > MAX_MESSAGE_SIZE {
        return Err(ParseError::MessageTooLarge {
            length: total_length,
            max: MAX_MESSAGE_SIZE,
        });
    }

    let total_length = total_length as usize;
    let header_length = header_length as usize;

    if buffer.len() < total_length {
        return Ok(None);
    }

    // Prelude CRC: covers first 8 bytes
    let actual_prelude_crc = crc32(&buffer[..8]);
    if actual_prelude_crc != prelude_crc {
        return Err(ParseError::PreludeCrcMismatch {
            expected: prelude_crc,
            actual: actual_prelude_crc,
        });
    }

    // Message CRC: covers everything except last 4 bytes
    let message_crc = u32::from_be_bytes([
        buffer[total_length - 4],
        buffer[total_length - 3],
        buffer[total_length - 2],
        buffer[total_length - 1],
    ]);
    let actual_message_crc = crc32(&buffer[..total_length - 4]);
    if actual_message_crc != message_crc {
        return Err(ParseError::MessageCrcMismatch {
            expected: message_crc,
            actual: actual_message_crc,
        });
    }

    // Parse headers
    let headers_start = PRELUDE_SIZE;
    let headers_end = headers_start + header_length;
    if headers_end > total_length - 4 {
        return Err(ParseError::HeaderParseFailed(
            "header length exceeds message boundary".to_string(),
        ));
    }
    let headers = parse_headers(&buffer[headers_start..headers_end], header_length)?;

    // Extract payload
    let payload_start = headers_end;
    let payload_end = total_length - 4;
    let payload = buffer[payload_start..payload_end].to_vec();

    Ok(Some((Frame { headers, payload }, total_length)))
}

// ---- Decoder state machine ----

const DEFAULT_MAX_BUFFER_SIZE: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_ERRORS: usize = 5;
const DEFAULT_BUFFER_CAPACITY: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderState {
    Ready,
    Parsing,
    Recovering,
    Stopped,
}

/// Streaming decoder for AWS EventStream binary frames.
///
/// Feed raw bytes via `feed()`, then call `decode()` to extract frames.
/// Implements automatic error recovery with a 5-consecutive-error limit.
pub struct EventStreamDecoder {
    buffer: BytesMut,
    state: DecoderState,
    frames_decoded: usize,
    error_count: usize,
    max_errors: usize,
    max_buffer_size: usize,
    pub bytes_skipped: usize,
}

impl Default for EventStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStreamDecoder {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUFFER_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: BytesMut::with_capacity(capacity),
            state: DecoderState::Ready,
            frames_decoded: 0,
            error_count: 0,
            max_errors: DEFAULT_MAX_ERRORS,
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
            bytes_skipped: 0,
        }
    }

    /// Feed raw bytes into the decoder buffer.
    pub fn feed(&mut self, data: &[u8]) -> ParseResult<()> {
        let new_size = self.buffer.len() + data.len();
        if new_size > self.max_buffer_size {
            return Err(ParseError::BufferOverflow {
                size: new_size,
                max: self.max_buffer_size,
            });
        }
        self.buffer.extend_from_slice(data);
        if self.state == DecoderState::Recovering {
            self.state = DecoderState::Ready;
        }
        Ok(())
    }

    /// Try to decode one frame from the buffer.
    /// Returns `Ok(None)` if not enough data.
    pub fn decode(&mut self) -> ParseResult<Option<Frame>> {
        if self.state == DecoderState::Stopped {
            return Err(ParseError::TooManyErrors {
                count: self.error_count,
                last_error: "decoder stopped".to_string(),
            });
        }
        if self.buffer.is_empty() {
            self.state = DecoderState::Ready;
            return Ok(None);
        }

        self.state = DecoderState::Parsing;

        match parse_frame(&self.buffer) {
            Ok(Some((frame, consumed))) => {
                self.buffer.advance(consumed);
                self.state = DecoderState::Ready;
                self.frames_decoded += 1;
                self.error_count = 0;
                Ok(Some(frame))
            }
            Ok(None) => {
                self.state = DecoderState::Ready;
                Ok(None)
            }
            Err(e) => {
                self.error_count += 1;
                if self.error_count >= self.max_errors {
                    self.state = DecoderState::Stopped;
                    error!(
                        error_count = self.error_count,
                        error = %e,
                        "EventStream 解码器连续错误过多，已停止"
                    );
                    return Err(ParseError::TooManyErrors {
                        count: self.error_count,
                        last_error: e.to_string(),
                    });
                }
                warn!(
                    error_count = self.error_count,
                    error = %e,
                    "EventStream 解析错误，尝试恢复"
                );
                self.try_recover(&e);
                self.state = DecoderState::Recovering;
                Err(e)
            }
        }
    }

    fn try_recover(&mut self, error: &ParseError) {
        match error {
            ParseError::PreludeCrcMismatch { .. }
            | ParseError::MessageTooSmall { .. }
            | ParseError::MessageTooLarge { .. } => {
                // Prelude error: skip 1 byte to re-align
                if !self.buffer.is_empty() {
                    self.buffer.advance(1);
                    self.bytes_skipped += 1;
                }
            }
            ParseError::MessageCrcMismatch { .. } | ParseError::HeaderParseFailed(_) => {
                // Data error: try to skip the entire frame using total_length from prelude
                if self.buffer.len() >= 4 {
                    let total_length =
                        u32::from_be_bytes([
                            self.buffer[0],
                            self.buffer[1],
                            self.buffer[2],
                            self.buffer[3],
                        ]) as usize;
                    if total_length >= MIN_MESSAGE_SIZE && total_length <= self.buffer.len() {
                        self.buffer.advance(total_length);
                        self.bytes_skipped += total_length;
                        return;
                    }
                }
                // Fallback: skip 1 byte
                if !self.buffer.is_empty() {
                    self.buffer.advance(1);
                    self.bytes_skipped += 1;
                }
            }
            _ => {
                if !self.buffer.is_empty() {
                    self.buffer.advance(1);
                    self.bytes_skipped += 1;
                }
            }
        }
    }

    /// Returns true if the decoder has been stopped due to too many errors.
    pub fn is_stopped(&self) -> bool {
        self.state == DecoderState::Stopped
    }

    /// Returns the number of successfully decoded frames.
    pub fn frames_decoded(&self) -> usize {
        self.frames_decoded
    }
}


// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn parse_frame_needs_more_data() {
        let buf = [0u8; 8]; // less than PRELUDE_SIZE
        assert!(parse_frame(&buf).unwrap().is_none());
    }

    #[test]
    fn parse_frame_message_too_small() {
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&5u32.to_be_bytes()); // total_length = 5 < 16
        let result = parse_frame(&buf);
        assert!(matches!(result, Err(ParseError::MessageTooSmall { .. })));
    }

    #[test]
    fn roundtrip_simple_frame() {
        // Build a minimal frame with no headers and empty payload
        let total_length: u32 = 16; // prelude(12) + msg_crc(4)
        let header_length: u32 = 0;
        let mut frame_bytes = Vec::new();
        frame_bytes.extend_from_slice(&total_length.to_be_bytes());
        frame_bytes.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&frame_bytes[..8]);
        frame_bytes.extend_from_slice(&prelude_crc.to_be_bytes());
        // No headers, no payload
        let msg_crc = crc32(&frame_bytes[..12]);
        frame_bytes.extend_from_slice(&msg_crc.to_be_bytes());

        let result = parse_frame(&frame_bytes).unwrap().unwrap();
        assert_eq!(result.1, 16); // consumed 16 bytes
        assert!(result.0.payload.is_empty());
    }

    #[test]
    fn decoder_feed_and_decode() {
        let mut decoder = EventStreamDecoder::new();

        // Build a simple frame
        let total_length: u32 = 16;
        let header_length: u32 = 0;
        let mut frame_bytes = Vec::new();
        frame_bytes.extend_from_slice(&total_length.to_be_bytes());
        frame_bytes.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&frame_bytes[..8]);
        frame_bytes.extend_from_slice(&prelude_crc.to_be_bytes());
        let msg_crc = crc32(&frame_bytes[..12]);
        frame_bytes.extend_from_slice(&msg_crc.to_be_bytes());

        decoder.feed(&frame_bytes).unwrap();
        let frame = decoder.decode().unwrap().unwrap();
        assert!(frame.payload.is_empty());
        assert_eq!(decoder.frames_decoded(), 1);

        // No more frames
        assert!(decoder.decode().unwrap().is_none());
    }

    #[test]
    fn decoder_recovery_on_bad_frame() {
        let mut decoder = EventStreamDecoder::new();
        // Feed garbage
        decoder.feed(&[0xFF; 20]).unwrap();
        // Should fail but recover
        let result = decoder.decode();
        assert!(result.is_err());
        assert!(!decoder.is_stopped());
    }

    #[test]
    fn header_string_value() {
        let mut headers = Headers::new();
        headers.insert(
            ":event-type".to_string(),
            HeaderValue::String("assistantResponseEvent".to_string()),
        );
        assert_eq!(
            headers.event_type(),
            Some("assistantResponseEvent")
        );
    }

    #[test]
    fn event_from_assistant_response_frame() {
        let payload = serde_json::json!({"content": "Hello, world!"});
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let header_bytes = {
            // Build header: one string header ":event-type" = "assistantResponseEvent"
            let name = b":event-type";
            let value = b"assistantResponseEvent";
            let mut h = Vec::new();
            h.push(name.len() as u8);
            h.extend_from_slice(name);
            h.push(7u8); // String type
            h.extend_from_slice(&(value.len() as u16).to_be_bytes());
            h.extend_from_slice(value);
            h
        };

        let total_length =
            (PRELUDE_SIZE + header_bytes.len() + payload_bytes.len() + 4) as u32;
        let header_length = header_bytes.len() as u32;

        let mut frame_bytes = Vec::new();
        frame_bytes.extend_from_slice(&total_length.to_be_bytes());
        frame_bytes.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&frame_bytes[..8]);
        frame_bytes.extend_from_slice(&prelude_crc.to_be_bytes());
        frame_bytes.extend_from_slice(&header_bytes);
        frame_bytes.extend_from_slice(&payload_bytes);
        let msg_crc = crc32(&frame_bytes[..total_length as usize - 4]);
        frame_bytes.extend_from_slice(&msg_crc.to_be_bytes());

        let (frame, _) = parse_frame(&frame_bytes).unwrap().unwrap();
        let event = Event::from_frame(&frame).unwrap();
        match event {
            Event::AssistantResponse { content } => {
                assert_eq!(content, "Hello, world!");
            }
            _ => panic!("Expected AssistantResponse"),
        }
    }

    #[test]
    fn text_fallback_parses_assistant_response() {
        // Simulate corrupted binary framing — just JSON objects in a byte stream
        let json_data = r#"{"assistantResponseEvent":{"content":"Hello from text fallback"}}"#;
        let events = try_parse_text_events(json_data.as_bytes());
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::AssistantResponse { content } => assert_eq!(content, "Hello from text fallback"),
            _ => panic!("Expected AssistantResponse"),
        }
    }

    #[test]
    fn text_fallback_parses_tool_use() {
        let json_data = r#"{"toolUseEvent":{"toolUseId":"toolu_123","name":"bash","input":"{\"cmd\":\"ls\"}","stop":true}}"#;
        let events = try_parse_text_events(json_data.as_bytes());
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ToolUse { tool_use_id, name, stop, .. } => {
                assert_eq!(tool_use_id, "toolu_123");
                assert_eq!(name, "bash");
                assert!(*stop);
            }
            _ => panic!("Expected ToolUse"),
        }
    }

    #[test]
    fn text_fallback_empty_on_binary_garbage() {
        let garbage = vec![0xFF, 0xFE, 0xFD, 0x00, 0x01, 0x02];
        let events = try_parse_text_events(&garbage);
        assert!(events.is_empty());
    }

    #[test]
    fn text_fallback_multiple_events() {
        let json_data = concat!(
            r#"{"assistantResponseEvent":{"content":"Part 1"}}"#,
            r#"{"assistantResponseEvent":{"content":"Part 2"}}"#,
        );
        let events = try_parse_text_events(json_data.as_bytes());
        assert_eq!(events.len(), 2);
    }
}
