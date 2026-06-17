//! Upstream endpoint URL construction for OpenAI and Anthropic providers.

pub fn openai_chat_completions_url(base_url: &str) -> String {
    let base_url = base_url.trim();
    if path_has_suffix(base_url, &["chat", "completions"]) {
        return base_url.to_string();
    }

    let endpoint = if has_openai_api_prefix(base_url) {
        "chat/completions"
    } else {
        "v1/chat/completions"
    };
    append_endpoint(base_url, endpoint)
}

pub fn anthropic_messages_url(base_url: &str) -> String {
    let base_url = base_url.trim();
    if path_has_suffix(base_url, &["messages"]) {
        return base_url.to_string();
    }

    let endpoint = if path_segments(base_url)
        .iter()
        .any(|segment| segment == "v1")
    {
        "messages"
    } else {
        "v1/messages"
    };
    append_endpoint(base_url, endpoint)
}

fn append_endpoint(base_url: &str, endpoint: &str) -> String {
    let (path_part, suffix) = split_url_suffix(base_url.trim_end_matches('/'));
    format!("{}/{}{}", path_part.trim_end_matches('/'), endpoint, suffix)
}

fn has_openai_api_prefix(base_url: &str) -> bool {
    path_segments(base_url)
        .iter()
        .any(|segment| segment == "openai" || segment == "v1" || segment.starts_with("v1beta"))
}

fn path_has_suffix(base_url: &str, suffix: &[&str]) -> bool {
    let segments = path_segments(base_url);
    segments.len() >= suffix.len()
        && segments[segments.len() - suffix.len()..]
            .iter()
            .zip(suffix.iter())
            .all(|(segment, expected)| segment == expected)
}

fn path_segments(base_url: &str) -> Vec<String> {
    let (without_suffix, _) = split_url_suffix(base_url);
    let path = if let Some((_, rest)) = without_suffix.split_once("://") {
        rest.find('/').map(|idx| &rest[idx..]).unwrap_or("")
    } else {
        without_suffix
    };

    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_ascii_lowercase())
        .collect()
}

fn split_url_suffix(url: &str) -> (&str, &str) {
    let suffix_index = url
        .char_indices()
        .find(|(_, ch)| *ch == '?' || *ch == '#')
        .map(|(idx, _)| idx);

    match suffix_index {
        Some(idx) => (&url[..idx], &url[idx..]),
        None => (url, ""),
    }
}

#[cfg(test)]
mod url_tests {
    use super::{anthropic_messages_url, openai_chat_completions_url};

    #[test]
    fn openai_url_adds_v1_for_host_root() {
        assert_eq!(
            openai_chat_completions_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
    }

    #[test]
    fn openai_url_does_not_duplicate_v1_prefix() {
        assert_eq!(
            openai_chat_completions_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn openai_url_preserves_openai_compatible_vendor_prefix() {
        assert_eq!(
            openai_chat_completions_url("https://generativelanguage.googleapis.com/v1beta/openai"),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
    }

    #[test]
    fn openai_url_preserves_query_string_after_endpoint() {
        assert_eq!(
            openai_chat_completions_url(
                "https://example.openai.azure.com/openai/deployments/demo?api-version=2024-10-21"
            ),
            "https://example.openai.azure.com/openai/deployments/demo/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn anthropic_url_does_not_duplicate_v1_prefix() {
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
    }
}
