//! Thinking tag FSM parser for streaming responses.
//!
//! Detects `<thinking>`, `<think>`, `<reasoning>`, `<thought>` tags at the
//! start of a response and separates thinking content from regular content.
//!
//! Supports 4 output modes:
//! - `as_reasoning_content`: Extract thinking to reasoning_content field
//! - `remove`: Remove thinking content entirely
//! - `pass`: Pass through with original tags in content
//! - `strip_tags`: Remove tags but keep content

use tracing::debug;

/// FSM states for thinking tag detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    /// Buffering initial content to detect opening tag
    PreContent,
    /// Inside a thinking block, buffering until closing tag
    InThinking,
    /// Regular streaming, no more thinking detection
    Streaming,
}

/// Processing mode for thinking content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingHandlingMode {
    /// Extract to reasoning_content/thinking field (default)
    AsReasoningContent,
    /// Remove thinking content entirely
    Remove,
    /// Pass through with original tags in content
    Pass,
    /// Remove tags but keep content
    StripTags,
}

impl ThinkingHandlingMode {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "as_reasoning_content" => Self::AsReasoningContent,
            "remove" => Self::Remove,
            "pass" => Self::Pass,
            "strip_tags" => Self::StripTags,
            _ => Self::AsReasoningContent,
        }
    }
}

/// Output from the thinking parser.
#[derive(Debug, Clone)]
pub enum ThinkingOutput {
    /// Thinking content to send to reasoning_content field
    ThinkingDelta(String),
    /// Regular content delta
    ContentDelta(String),
    /// No output (buffering or removed)
    None,
}

/// FSM-based thinking tag parser for streaming content.
pub struct ThinkingParser {
    state: ParserState,
    mode: ThinkingHandlingMode,
    /// Buffer for tag detection (pre-content) and cautious sending (in-thinking)
    buffer: String,
    /// The opening tag that was detected (e.g., "<thinking>")
    open_tag: String,
    /// The corresponding closing tag
    close_tag: String,
    /// Max buffer size for initial tag detection
    initial_buffer_size: usize,
    /// Max tag length * 2 for cautious buffering
    cautious_buffer_size: usize,
    /// Accumulated thinking content for pass/strip_tags modes
    thinking_content: String,
}

/// Check if the text is inside a code fence (odd number of ``` backtick groups).
/// An odd count means we're currently inside an open code fence.
fn is_in_code_fence(text: &str) -> bool {
    let mut count = 0;
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            // Only count triple (or longer) backtick fences
            if i - start >= 3 {
                count += 1;
            }
        } else {
            i += 1;
        }
    }
    count % 2 == 1
}

impl ThinkingParser {
    pub fn new(mode: ThinkingHandlingMode) -> Self {
        Self {
            state: ParserState::PreContent,
            mode,
            buffer: String::new(),
            open_tag: String::new(),
            close_tag: String::new(),
            initial_buffer_size: 20,
            cautious_buffer_size: 30, // max("<thinking>", "<reasoning>") * 2 + margin
            thinking_content: String::new(),
        }
    }

    /// Feed a text chunk and get output chunks.
    pub fn feed(&mut self, text: &str) -> Vec<ThinkingOutput> {
        match self.state {
            ParserState::PreContent => self.handle_pre_content(text),
            ParserState::InThinking => self.handle_in_thinking(text),
            ParserState::Streaming => vec![ThinkingOutput::ContentDelta(text.to_string())],
        }
    }

    /// Finalize the parser, flushing any remaining buffer.
    pub fn finalize(&mut self) -> Vec<ThinkingOutput> {
        let mut outputs = Vec::new();

        match self.state {
            ParserState::PreContent => {
                // Never detected a tag — emit buffered content as regular text
                if !self.buffer.is_empty() {
                    outputs.push(ThinkingOutput::ContentDelta(self.buffer.clone()));
                    self.buffer.clear();
                }
            }
            ParserState::InThinking => {
                // Stream ended while still in thinking — flush remaining
                if !self.buffer.is_empty() {
                    match self.mode {
                        ThinkingHandlingMode::AsReasoningContent => {
                            outputs.push(ThinkingOutput::ThinkingDelta(self.buffer.clone()));
                        }
                        ThinkingHandlingMode::Pass => {
                            self.thinking_content.push_str(&self.buffer);
                        }
                        ThinkingHandlingMode::StripTags => {
                            outputs.push(ThinkingOutput::ContentDelta(self.buffer.clone()));
                        }
                        ThinkingHandlingMode::Remove => {}
                    }
                    self.buffer.clear();
                }
                // For pass mode, emit the full thinking block with tags
                if self.mode == ThinkingHandlingMode::Pass && !self.thinking_content.is_empty() {
                    let tagged = format!("{}{}{}", self.open_tag, self.thinking_content, self.close_tag);
                    outputs.push(ThinkingOutput::ContentDelta(tagged));
                }
            }
            ParserState::Streaming => {}
        }

        self.state = ParserState::Streaming;
        outputs
    }

    /// Handle content in the PreContent state — buffering to detect opening tag.
    fn handle_pre_content(&mut self, text: &str) -> Vec<ThinkingOutput> {
        self.buffer.push_str(text);

        // Strip leading whitespace for tag detection
        let trimmed = self.buffer.trim_start().to_string();

        // Check for opening tags
        let open_tags = ["<thinking>", "<think>", "<reasoning>", "<thought>"];
        let mut found_tag = None;

        for tag in &open_tags {
            if trimmed.starts_with(tag) {
                // Quote detection: skip tag if preceded by an odd number of backticks
                // (meaning we're inside a ``` code fence where <thinking> is literal text)
                if is_in_code_fence(&self.buffer) {
                    debug!(tag = tag, "thinking 标签在代码围栏内，跳过");
                    break;
                }
                found_tag = Some(*tag);
                break;
            }
        }

        if let Some(tag) = found_tag {
            // Found opening tag!
            self.open_tag = tag.to_string();
            self.close_tag = format!("</{}", &tag[1..]);
            self.state = ParserState::InThinking;
            self.buffer.clear();

            debug!(tag = tag, "检测到 thinking 开始标签");

            // Process remaining content after the tag through InThinking handler
            let after_tag = &trimmed[tag.len()..];
            if after_tag.is_empty() {
                return vec![];
            }
            return self.handle_in_thinking(after_tag);
        }

        // Check if we've exceeded the initial buffer without finding a tag
        if self.buffer.len() > self.initial_buffer_size {
            // No tag found — this is regular content
            self.state = ParserState::Streaming;
            let content = self.buffer.clone();
            self.buffer.clear();
            return vec![ThinkingOutput::ContentDelta(content)];
        }

        // Still buffering
        vec![]
    }

    /// Handle content in the InThinking state — cautious buffering for closing tag.
    fn handle_in_thinking(&mut self, text: &str) -> Vec<ThinkingOutput> {
        self.buffer.push_str(text);
        let mut outputs = Vec::new();

        // Check for closing tag
        if let Some(close_pos) = self.buffer.find(&self.close_tag) {
            // Found closing tag — emit thinking content before it
            let thinking_text = &self.buffer[..close_pos];
            let after_close = self.buffer[close_pos + self.close_tag.len()..].to_string();

            if !thinking_text.is_empty() {
                match self.mode {
                    ThinkingHandlingMode::AsReasoningContent => {
                        outputs.push(ThinkingOutput::ThinkingDelta(thinking_text.to_string()));
                    }
                    ThinkingHandlingMode::Pass => {
                        self.thinking_content.push_str(thinking_text);
                    }
                    ThinkingHandlingMode::StripTags => {
                        outputs.push(ThinkingOutput::ContentDelta(thinking_text.to_string()));
                    }
                    ThinkingHandlingMode::Remove => {}
                }
            }

            // For pass mode, emit the full tagged block
            if self.mode == ThinkingHandlingMode::Pass && !self.thinking_content.is_empty() {
                let tagged = format!("{}{}{}", self.open_tag, self.thinking_content, self.close_tag);
                outputs.push(ThinkingOutput::ContentDelta(tagged));
                self.thinking_content.clear();
            }

            // Transition to Streaming
            self.state = ParserState::Streaming;
            self.buffer.clear();

            // Emit content after closing tag as regular text
            if !after_close.is_empty() {
                outputs.push(ThinkingOutput::ContentDelta(after_close.to_string()));
            }

            return outputs;
        }

        // No closing tag yet — use cautious buffering
        // Keep last N chars in buffer in case the closing tag spans chunks
        let safe_len = self.buffer.len().saturating_sub(self.cautious_buffer_size);
        if safe_len > 0 {
            let safe_content = self.buffer[..safe_len].to_string();
            self.buffer = self.buffer[safe_len..].to_string();

            match self.mode {
                ThinkingHandlingMode::AsReasoningContent => {
                    outputs.push(ThinkingOutput::ThinkingDelta(safe_content));
                }
                ThinkingHandlingMode::Pass => {
                    self.thinking_content.push_str(&safe_content);
                }
                ThinkingHandlingMode::StripTags => {
                    outputs.push(ThinkingOutput::ContentDelta(safe_content));
                }
                ThinkingHandlingMode::Remove => {}
            }
        }

        outputs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_thinking_tag() {
        let mut parser = ThinkingParser::new(ThinkingHandlingMode::AsReasoningContent);
        let outputs = parser.feed("Hello, world!");
        let mut final_outputs = parser.finalize();
        let mut all = outputs;
        all.append(&mut final_outputs);

        // Should all be content deltas
        assert!(all.iter().all(|o| matches!(o, ThinkingOutput::ContentDelta(_))));
    }

    #[test]
    fn test_thinking_tag_as_reasoning() {
        let mut parser = ThinkingParser::new(ThinkingHandlingMode::AsReasoningContent);
        let mut all = Vec::new();

        all.extend(parser.feed("<thinking>"));
        all.extend(parser.feed("let me think..."));
        all.extend(parser.feed("</thinking>\n\n"));
        all.extend(parser.feed("Here is my answer."));
        all.extend(parser.finalize());

        let thinking: Vec<_> = all.iter().filter(|o| matches!(o, ThinkingOutput::ThinkingDelta(_))).collect();
        let content: Vec<_> = all.iter().filter(|o| matches!(o, ThinkingOutput::ContentDelta(_))).collect();

        assert!(!thinking.is_empty());
        assert!(!content.is_empty());
    }

    #[test]
    fn test_thinking_tag_remove() {
        let mut parser = ThinkingParser::new(ThinkingHandlingMode::Remove);
        let mut all = Vec::new();

        all.extend(parser.feed("<thinking>secret thoughts</thinking>\n\nvisible text"));
        all.extend(parser.finalize());

        let content: Vec<_> = all.iter().filter(|o| matches!(o, ThinkingOutput::ContentDelta(_))).collect();
        let thinking: Vec<_> = all.iter().filter(|o| matches!(o, ThinkingOutput::ThinkingDelta(_))).collect();

        assert!(thinking.is_empty()); // Removed
        assert!(!content.is_empty());
    }

    #[test]
    fn test_thinking_tag_strip_tags() {
        let mut parser = ThinkingParser::new(ThinkingHandlingMode::StripTags);
        let mut all = Vec::new();

        all.extend(parser.feed("<thinking>my thoughts</thinking>\n\nanswer"));
        all.extend(parser.finalize());

        let content: String = all
            .iter()
            .filter_map(|o| match o {
                ThinkingOutput::ContentDelta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();

        assert!(content.contains("my thoughts"));
        assert!(content.contains("answer"));
        assert!(!content.contains("<thinking>"));
    }

    #[test]
    fn test_thinking_tag_pass() {
        let mut parser = ThinkingParser::new(ThinkingHandlingMode::Pass);
        let mut all = Vec::new();

        all.extend(parser.feed("<thinking>thoughts</thinking>\n\nanswer"));
        all.extend(parser.finalize());

        let content: String = all
            .iter()
            .filter_map(|o| match o {
                ThinkingOutput::ContentDelta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();

        assert!(content.contains("<thinking>"));
        assert!(content.contains("</thinking>"));
        assert!(content.contains("answer"));
    }

    #[test]
    fn test_think_tag() {
        let mut parser = ThinkingParser::new(ThinkingHandlingMode::AsReasoningContent);
        let mut all = Vec::new();

        all.extend(parser.feed("<think>short thought</think>answer"));
        all.extend(parser.finalize());

        let thinking: Vec<_> = all.iter().filter(|o| matches!(o, ThinkingOutput::ThinkingDelta(_))).collect();
        assert!(!thinking.is_empty());
    }

    #[test]
    fn test_split_closing_tag() {
        let mut parser = ThinkingParser::new(ThinkingHandlingMode::AsReasoningContent);
        let mut all = Vec::new();

        // Split the closing tag across chunks
        all.extend(parser.feed("<thinking>deep thoughts"));
        all.extend(parser.feed(" continued</th"));
        all.extend(parser.feed("continued</th"));
        all.extend(parser.feed("inking>\n\nanswer"));
        all.extend(parser.finalize());

        let thinking: Vec<_> = all.iter().filter(|o| matches!(o, ThinkingOutput::ThinkingDelta(_))).collect();
        assert!(!thinking.is_empty());
    }

    #[test]
    fn test_thinking_in_code_fence_is_ignored() {
        // When response starts with ``` followed by <thinking>, it's literal text
        let mut parser = ThinkingParser::new(ThinkingHandlingMode::AsReasoningContent);
        let mut all = Vec::new();

        all.extend(parser.feed("```xml\n<thinking>this is code"));
        all.extend(parser.feed("\n```\n\nActual answer."));
        all.extend(parser.finalize());

        // Everything should be content (thinking tag inside code fence is ignored)
        let thinking: Vec<_> = all.iter().filter(|o| matches!(o, ThinkingOutput::ThinkingDelta(_))).collect();
        let content: Vec<_> = all.iter().filter(|o| matches!(o, ThinkingOutput::ContentDelta(_))).collect();
        assert!(thinking.is_empty(), "thinking inside code fence should be ignored");
        assert!(!content.is_empty());
    }

    #[test]
    fn test_is_in_code_fence() {
        assert!(!is_in_code_fence("hello"));
        assert!(!is_in_code_fence("```\nclosed\n```"));
        assert!(is_in_code_fence("```\nopen fence"));
        assert!(is_in_code_fence("```\n```\n```\nodd-count-open"));
        assert!(!is_in_code_fence("```\n```\n```\n```\neven-count-closed"));
        // Single backticks don't count as fences
        assert!(!is_in_code_fence("`code`"));
    }
}
