/// Truncate a string to at most `max_bytes` bytes at a valid UTF-8 boundary.
/// If truncation occurs, appends "[truncated]" marker.
///
/// If the string length in bytes is <= `max_bytes`, returns the string unchanged.
pub fn truncate_body(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let boundary = s.floor_char_boundary(max_bytes);
    let mut result = s[..boundary].to_string();
    result.push_str("[truncated]");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_truncation_when_within_limit() {
        let input = "hello world";
        let result = truncate_body(input, 100);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn no_truncation_when_exactly_at_limit() {
        let input = "hello"; // 5 bytes
        let result = truncate_body(input, 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncates_and_appends_marker() {
        let input = "hello world"; // 11 bytes
        let result = truncate_body(input, 5);
        assert_eq!(result, "hello[truncated]");
    }

    #[test]
    fn truncates_at_utf8_boundary() {
        // 'é' is 2 bytes (U+00E9), so "café" = [99, 97, 102, 195, 169] = 5 bytes
        let input = "café";
        // Truncating at 4 bytes would split 'é', so floor_char_boundary(4) = 3
        let result = truncate_body(input, 4);
        assert_eq!(result, "caf[truncated]");
    }

    #[test]
    fn truncates_multibyte_emoji() {
        // '🦀' is 4 bytes
        let input = "hi🦀bye";
        // "hi" = 2 bytes, "🦀" = 4 bytes, "bye" = 3 bytes => total 9 bytes
        // Truncating at 3 bytes: floor_char_boundary(3) = 2 (can't fit the emoji)
        let result = truncate_body(input, 3);
        assert_eq!(result, "hi[truncated]");
    }

    #[test]
    fn empty_string_no_truncation() {
        let result = truncate_body("", 100);
        assert_eq!(result, "");
    }

    #[test]
    fn max_bytes_zero_truncates_everything() {
        let input = "hello";
        let result = truncate_body(input, 0);
        assert_eq!(result, "[truncated]");
    }
}
