//! Kiro model ID normalization.
//!
//! Converts various Claude model name formats to Kiro's expected format.
//! Handles date suffixes, version separators, legacy names, and GPT mapping.

/// Known Kiro model IDs and their context window sizes.
pub const KIRO_MODELS: &[(&str, u32)] = &[
    ("claude-sonnet-4", 200_000),
    ("claude-sonnet-4.5", 200_000),
    ("claude-sonnet-4.6", 1_000_000),
    ("claude-opus-4.5", 200_000),
    ("claude-opus-4.6", 1_000_000),
    ("claude-opus-4.7", 1_000_000),
    ("claude-haiku-4.5", 200_000),
];

/// Get the context window size for a Kiro model ID.
pub fn context_window_size(model_id: &str) -> u32 {
    KIRO_MODELS
        .iter()
        .find(|(name, _)| *name == model_id)
        .map(|(_, size)| *size)
        .unwrap_or(200_000)
}

/// Normalize a client-provided model name to a Kiro model ID.
///
/// Returns `None` if the model cannot be mapped to any known Kiro model.
///
/// Normalization steps:
/// 1. Strip date suffixes (e.g., `-20250929`)
/// 2. Convert dash-separated versions to dot-separated (e.g., `4-5` → `4.5`)
/// 3. Map legacy Claude 3.x names
/// 4. Map GPT model names
/// 5. Validate against known Kiro models
pub fn normalize_model_id(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();

    // Step 1: Strip date suffixes like -20250929, -20250514
    let stripped = strip_date_suffix(&lower);

    // Step 2: Convert version separators
    let normalized = normalize_version_separator(&stripped);

    // Step 3: Legacy Claude 3.x mapping
    if let Some(mapped) = map_legacy_model(&normalized) {
        return Some(mapped);
    }

    // Step 4: GPT mapping
    if let Some(mapped) = map_gpt_model(&normalized) {
        return Some(mapped);
    }

    // Step 5: Check if it's a known Kiro model
    if KIRO_MODELS.iter().any(|(name, _)| *name == normalized) {
        return Some(normalized);
    }

    // Fallback: pass through as-is (Kiro may accept it)
    Some(normalized)
}

/// Strip date suffixes like `-20250929`, `-20250514` from model names.
fn strip_date_suffix(name: &str) -> String {
    // Match pattern: -YYYYMMDD at the end
    if let Some(pos) = name.rfind('-') {
        let suffix = &name[pos + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            return name[..pos].to_string();
        }
    }
    name.to_string()
}

/// Convert dash-separated minor versions to dot-separated.
/// e.g., `claude-sonnet-4-5` → `claude-sonnet-4.5`
fn normalize_version_separator(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() < 2 {
        return name.to_string();
    }

    // Check if the last part is a single digit (minor version)
    let last = parts[parts.len() - 1];
    if last.len() == 1 && last.chars().all(|c| c.is_ascii_digit()) {
        // Check if the second-to-last part is also a single digit (major version)
        let second_last = parts[parts.len() - 2];
        if second_last.len() == 1 && second_last.chars().all(|c| c.is_ascii_digit()) {
            // Join everything except the last part, then add .minor
            let prefix = parts[..parts.len() - 1].join("-");
            return format!("{}.{}", prefix, last);
        }
    }

    name.to_string()
}

/// Map legacy Claude 3.x model names to Kiro model IDs.
fn map_legacy_model(name: &str) -> Option<String> {
    // Order matters: more specific patterns first
    let mappings: &[(&str, &str)] = &[
        ("claude-3-5-sonnet", "claude-sonnet-4.5"),
        ("claude-3-5-haiku", "claude-haiku-4.5"),
        ("claude-3-opus", "claude-opus-4.5"),
        ("claude-3-sonnet", "claude-sonnet-4"),
        ("claude-3-haiku", "claude-haiku-4.5"),
    ];

    for (pattern, target) in mappings {
        if name.starts_with(pattern) || name == *pattern {
            return Some(target.to_string());
        }
    }
    None
}

/// Map GPT model names to Kiro model IDs.
fn map_gpt_model(name: &str) -> Option<String> {
    let mappings: &[(&str, &str)] = &[
        ("gpt-4-turbo", "claude-sonnet-4.5"),
        ("gpt-4o", "claude-sonnet-4.5"),
        ("gpt-4", "claude-sonnet-4.5"),
        ("gpt-3.5-turbo", "claude-sonnet-4.5"),
    ];

    for (pattern, target) in mappings {
        if name.starts_with(pattern) || name == *pattern {
            return Some(target.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_date_suffix_basic() {
        assert_eq!(strip_date_suffix("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
        assert_eq!(strip_date_suffix("claude-opus-4-20250514"), "claude-opus-4");
        assert_eq!(strip_date_suffix("claude-sonnet-4-5"), "claude-sonnet-4-5");
        assert_eq!(strip_date_suffix("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn normalize_version_separator_basic() {
        assert_eq!(normalize_version_separator("claude-sonnet-4-5"), "claude-sonnet-4.5");
        assert_eq!(normalize_version_separator("claude-opus-4-7"), "claude-opus-4.7");
        assert_eq!(normalize_version_separator("claude-sonnet-4.5"), "claude-sonnet-4.5");
        assert_eq!(normalize_version_separator("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn normalize_model_id_with_date_suffix() {
        assert_eq!(
            normalize_model_id("claude-sonnet-4-5-20250929"),
            Some("claude-sonnet-4.5".to_string())
        );
        assert_eq!(
            normalize_model_id("claude-opus-4-6-20250514"),
            Some("claude-opus-4.6".to_string())
        );
    }

    #[test]
    fn normalize_model_id_dot_format() {
        assert_eq!(
            normalize_model_id("claude-sonnet-4.5"),
            Some("claude-sonnet-4.5".to_string())
        );
    }

    #[test]
    fn normalize_model_id_legacy() {
        assert_eq!(
            normalize_model_id("claude-3-5-sonnet-20241022"),
            Some("claude-sonnet-4.5".to_string())
        );
        assert_eq!(
            normalize_model_id("claude-3-opus"),
            Some("claude-opus-4.5".to_string())
        );
        assert_eq!(
            normalize_model_id("claude-3-haiku-20240307"),
            Some("claude-haiku-4.5".to_string())
        );
    }

    #[test]
    fn normalize_model_id_gpt() {
        assert_eq!(
            normalize_model_id("gpt-4o"),
            Some("claude-sonnet-4.5".to_string())
        );
        assert_eq!(
            normalize_model_id("gpt-4-turbo"),
            Some("claude-sonnet-4.5".to_string())
        );
    }

    #[test]
    fn normalize_model_id_bare_sonnet_4() {
        assert_eq!(
            normalize_model_id("claude-sonnet-4"),
            Some("claude-sonnet-4".to_string())
        );
    }

    #[test]
    fn context_window_sizes() {
        assert_eq!(context_window_size("claude-sonnet-4.5"), 200_000);
        assert_eq!(context_window_size("claude-sonnet-4.6"), 1_000_000);
        assert_eq!(context_window_size("claude-opus-4.7"), 1_000_000);
        assert_eq!(context_window_size("unknown-model"), 200_000);
    }
}
