//! Model capability heuristics and reasoning-effort / JSON-schema helpers.

use serde_json::Value;

/// Heuristic: detect OpenAI o-series reasoning models (o1, o3, o4-mini, etc.).
/// Matches any model name starting with 'o' followed by a digit.
/// This may produce false positives for custom model names; use `quirks.supports_reasoning_effort`
/// to override behavior for non-OpenAI providers.
pub(crate) fn is_openai_o_series(model: &str) -> bool {
    model.len() > 1
        && model.starts_with('o')
        && model.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit())
}

/// Heuristic: detect models that support the `reasoning_effort` parameter.
/// Covers OpenAI o-series and GPT-5+. For non-OpenAI models, use
/// `quirks.supports_reasoning_effort = true` in the provider config instead.
pub(crate) fn supports_reasoning_effort(model: &str) -> bool {
    is_openai_o_series(model)
        || model
            .to_lowercase()
            .strip_prefix("gpt-")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_digit() && c >= '5')
}

pub(super) fn response_format_unavailable(model: &str) -> bool {
    model.to_ascii_lowercase().contains("deepseek")
}

pub(super) fn json_schema_instruction(schema: Option<&Value>) -> String {
    match schema.and_then(|schema| serde_json::to_string(schema).ok()) {
        Some(schema_str) => format!(
            "You must respond with a valid JSON object that strictly conforms to the following JSON Schema. Do not include any markdown code fences, explanations, or extra text outside the JSON object.\n\nSchema:\n{}",
            schema_str
        ),
        None => "Respond with a valid JSON object. Do not include any markdown code fences, explanations, or extra text outside the JSON object.".to_string(),
    }
}

pub(crate) fn resolve_reasoning_effort(body: &Value, max_effort: &str) -> Option<String> {
    if let Some(effort) = body
        .pointer("/output_config/effort")
        .and_then(|v| v.as_str())
    {
        return match effort {
            "low" => Some("low".into()),
            "medium" => Some("medium".into()),
            "high" => Some("high".into()),
            "max" => Some(max_effort.to_string()),
            _ => None,
        };
    }

    let thinking = body.get("thinking")?;
    match thinking.get("type").and_then(|t| t.as_str()) {
        Some("adaptive") => Some(max_effort.to_string()),
        Some("enabled") => {
            let budget = thinking
                .get("budget_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            match budget {
                0 => Some("high".into()),
                1..=3999 => Some("low".into()),
                4000..=15999 => Some("medium".into()),
                _ => Some("high".into()),
            }
        }
        Some("disabled") => None,
        _ => None,
    }
}
