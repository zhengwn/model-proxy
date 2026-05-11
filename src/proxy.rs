use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::Config,
    error::{AppError, Result},
};
use std::collections::{BTreeSet, HashMap};

const ANTHROPIC_BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";
const MAX_LOG_BODY_BYTES: usize = 4096;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> String {
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("req_{}_{}", millis, counter)
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

struct RequestCompletionGuard {
    request_id: String,
    request_start: Instant,
    phase: &'static str,
    completed: bool,
}

impl RequestCompletionGuard {
    fn new(request_id: String, request_start: Instant) -> Self {
        Self {
            request_id,
            request_start,
            phase: "received",
            completed: false,
        }
    }

    fn set_phase(&mut self, phase: &'static str) {
        self.phase = phase;
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RequestCompletionGuard {
    fn drop(&mut self) {
        if !self.completed {
            info!(
                request_id = self.request_id.as_str(),
                phase = self.phase,
                request_total_ms = elapsed_ms(self.request_start),
                "请求处理提前结束"
            );
        }
    }
}

/// 剥离开头的 x-anthropic-billing-header，防止 prefix cache 失效
fn strip_leading_anthropic_billing_header(text: &str) -> &str {
    if !text.starts_with(ANTHROPIC_BILLING_HEADER_PREFIX) {
        return text;
    }
    let Some(line_end) = text
        .as_bytes()
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
    else {
        return "";
    };
    let bytes = text.as_bytes();
    let mut rest_start = line_end + 1;
    if bytes[line_end] == b'\r' && bytes.get(line_end + 1) == Some(&b'\n') {
        rest_start += 1;
    }
    let rest = &text[rest_start..];
    rest
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: Client,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("构建 HTTP 客户端失败");

        Self {
            config: Arc::new(config),
            client,
        }
    }
}

/// 验证代理自身的 API key（如果配置了的话）
fn check_auth(headers: &HeaderMap, config: &Config) -> Result<()> {
    if let Some(expected_key) = &config.server.api_key {
        let provided = headers
            .get("x-api-key")
            .or_else(|| headers.get("authorization"))
            .and_then(|v| v.to_str().ok());

        let provided_clean = provided.map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim());

        if provided_clean != Some(expected_key.as_str()) {
            warn!("API key 验证失败");
            return Err(AppError::Unauthorized);
        }
    }
    Ok(())
}

/// 检测 OpenAI o-series 模型
fn is_openai_o_series(model: &str) -> bool {
    model.len() > 1
        && model.starts_with('o')
        && model.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit())
}

/// 检测是否支持 reasoning_effort
fn supports_reasoning_effort(model: &str) -> bool {
    is_openai_o_series(model)
        || model
            .to_lowercase()
            .strip_prefix("gpt-")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_digit() && c >= '5')
}

/// 从 Anthropic 请求解析 reasoning_effort
/// Priority: output_config.effort > thinking.type + budget_tokens
fn resolve_reasoning_effort(body: &Value, max_effort: &str) -> Option<String> {
    // Priority 1: output_config.effort
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

    // Priority 2: thinking.type + budget_tokens
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

/// 安全追加 UTF-8 字节到 buffer，处理跨 chunk 截断的 UTF-8 字符
fn append_utf8_safe(buffer: &mut String, remainder: &mut Vec<u8>, bytes: &[u8]) {
    remainder.extend_from_slice(bytes);
    match String::from_utf8(std::mem::take(remainder)) {
        Ok(text) => {
            buffer.push_str(&text);
        }
        Err(e) => {
            let valid = e.utf8_error().valid_up_to();
            let bytes = e.into_bytes();
            buffer.push_str(&String::from_utf8_lossy(&bytes[..valid]));
            *remainder = bytes[valid..].to_vec();
        }
    }
}

fn find_sse_block_end(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|pos| (pos, 2));
    let crlf = buffer.find("\r\n\r\n").map(|pos| (pos, 4));

    match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(if lf.0 < crlf.0 { lf } else { crlf }),
        (Some(lf), None) => Some(lf),
        (None, Some(crlf)) => Some(crlf),
        (None, None) => None,
    }
}

fn message_count(body: &Value) -> usize {
    body.get("messages")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0)
}

fn tool_count(body: &Value) -> usize {
    body.get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0)
}

fn truncate_for_log(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}...[truncated {} bytes]",
        &text[..end],
        text.len().saturating_sub(end)
    )
}

/// 转换 Anthropic content 为 OpenAI content（保留数组格式，保留 cache_control）
fn convert_content(content: &Value) -> Value {
    match content {
        Value::String(s) => json!(s),
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for block in arr {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            let mut part = json!({
                                "type": "text",
                                "text": text
                            });
                            if let Some(cc) = block.get("cache_control") {
                                part["cache_control"] = cc.clone();
                            }
                            parts.push(part);
                        }
                    }
                    "image" => {
                        if let Some(source) = block.get("source") {
                            let media_type = source
                                .get("media_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("image/png");
                            let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
                            let url = format!("data:{};base64,{}", media_type, data);
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {"url": url}
                            }));
                        }
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                return json!(null);
            }
            if parts.len() == 1 {
                let has_cache_control = parts[0].get("cache_control").is_some();
                if !has_cache_control {
                    if let Some(text) = parts[0].get("text").and_then(|v| v.as_str()) {
                        return json!(text);
                    }
                }
            }
            json!(parts)
        }
        _ => json!(null),
    }
}

/// 清理 tool name，确保符合 OpenAI `^[a-zA-Z0-9_-]{1,64}$` 规范
/// 返回 (清理后的名称, 是否被修改)
fn sanitize_tool_name(name: &str, existing: &std::collections::HashSet<String>) -> (String, bool) {
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() > 64 {
        sanitized.truncate(64);
    }
    // 解决冲突
    let base = sanitized.clone();
    let mut counter = 2;
    while existing.contains(&sanitized) {
        let suffix = format!("_{}", counter);
        let max_base_len = 64usize.saturating_sub(suffix.len());
        let truncate_to = base.len().min(max_base_len);
        sanitized = format!("{}{}", &base[..truncate_to], suffix);
        counter += 1;
    }
    let modified = sanitized != name;
    (sanitized, modified)
}

/// 清理 JSON schema（移除不支持的 format）
pub fn clean_schema(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        if obj.get("format").and_then(|v| v.as_str()) == Some("uri") {
            obj.remove("format");
        }
        if let Some(properties) = obj.get_mut("properties").and_then(|v| v.as_object_mut()) {
            for (_, value) in properties.iter_mut() {
                *value = clean_schema(value.clone());
            }
        }
        if let Some(items) = obj.get_mut("items") {
            *items = clean_schema(items.clone());
        }
    }
    schema
}

/// 转换 Anthropic tools 为 OpenAI tools（保留 cache_control，清理 tool name）
fn convert_tools(tools: &Value, forward_map: &mut HashMap<String, String>) -> Value {
    use std::collections::HashSet;
    match tools.as_array() {
        Some(arr) => {
            let mut existing = HashSet::new();
            let openai_tools: Vec<Value> = arr
                .iter()
                .filter(|t| t.get("type").and_then(|v| v.as_str()) != Some("BatchTool"))
                .map(|tool| {
                    let raw_name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let (name, modified) = sanitize_tool_name(raw_name, &existing);
                    existing.insert(name.clone());
                    if modified {
                        forward_map.insert(name.clone(), raw_name.to_string());
                    }
                    let description = tool
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let input_schema = tool.get("input_schema").cloned().unwrap_or(json!({}));
                    let mut t = json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description,
                            "parameters": clean_schema(input_schema)
                        }
                    });
                    if let Some(cc) = tool.get("cache_control") {
                        t["cache_control"] = cc.clone();
                    }
                    t
                })
                .collect();
            json!(openai_tools)
        }
        None => json!([]),
    }
}

fn sanitized_tool_name<'a>(
    name: &'a str,
    original_to_sanitized: &'a HashMap<String, String>,
) -> &'a str {
    original_to_sanitized
        .get(name)
        .map(|s| s.as_str())
        .unwrap_or(name)
}

/// 转换 Anthropic tool_choice 为 OpenAI tool_choice
fn convert_tool_choice(
    tool_choice: &Value,
    original_to_sanitized: &HashMap<String, String>,
) -> Value {
    match tool_choice {
        Value::String(s) => match s.as_str() {
            "auto" => json!("auto"),
            "any" => json!("required"), // Claude "any" = 必须调工具，映射为 OpenAI required
            _ => json!("auto"),
        },
        Value::Object(obj) => {
            let choice_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("auto");
            match choice_type {
                "auto" => json!("auto"),
                "any" => json!("required"), // 映射为 required
                "tool" => {
                    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let name = sanitized_tool_name(name, original_to_sanitized);
                    json!({
                        "type": "function",
                        "function": {
                            "name": name
                        }
                    })
                }
                _ => json!("auto"),
            }
        }
        _ => json!("auto"),
    }
}

/// Anthropic tool_use_id -> OpenAI tool_call_id
fn anthropic_id_to_openai(id: &str) -> String {
    if id.starts_with("toolu_") {
        format!("call_{}", &id[6..])
    } else {
        format!("call_{}", id)
    }
}

/// OpenAI tool_call_id -> Anthropic tool_use_id
fn openai_id_to_anthropic(id: &str) -> String {
    if id.starts_with("call_") {
        format!("toolu_{}", &id[5..])
    } else {
        format!("toolu_{}", id)
    }
}

/// 将 Anthropic 请求体转换为 OpenAI 请求体
fn anthropic_to_openai(body: Value, config: &Config) -> (Value, HashMap<String, String>) {
    let quirks = &config.provider.quirks;
    let mut openai = json!({});

    // model
    openai["model"] = json!(config.provider.model.clone());

    // tools 需要先转换，后续历史 tool_use / tool_choice 会复用同一套清理后的名称
    let mut forward_map = HashMap::new();
    if let Some(tools) = body.get("tools") {
        openai["tools"] = convert_tools(tools, &mut forward_map);
    }
    let original_to_sanitized: HashMap<String, String> = forward_map
        .iter()
        .map(|(sanitized, original)| (original.clone(), sanitized.clone()))
        .collect();

    // messages
    let mut messages = Vec::new();

    // system 字段 -> system message（剥离开头 billing header 防 cache 失效）
    if let Some(system) = body.get("system") {
        match system {
            Value::String(text) => {
                let text = strip_leading_anthropic_billing_header(text);
                if !text.is_empty() {
                    messages.push(json!({"role": "system", "content": text}));
                }
            }
            Value::Array(arr) => {
                let mut is_first = true;
                for block in arr {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        let text = if is_first {
                            is_first = false;
                            strip_leading_anthropic_billing_header(text)
                        } else {
                            text
                        };
                        if text.is_empty() {
                            continue;
                        }
                        let mut sys_msg = json!({"role": "system", "content": text});
                        if let Some(cc) = block.get("cache_control") {
                            sys_msg["cache_control"] = cc.clone();
                        }
                        messages.push(sys_msg);
                    }
                }
            }
            _ => {}
        }
    }

    // messages 数组
    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in arr {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = msg.get("content").cloned().unwrap_or(json!(""));

            // 处理 tool_result
            if role == "user" {
                if let Some(content_arr) = content.as_array() {
                    let mut tool_results = Vec::new();
                    let mut non_tool_blocks = Vec::new();
                    let mut has_tool_result = false;
                    for block in content_arr {
                        let block_type = block.get("type").and_then(|v| v.as_str());
                        if block_type == Some("tool_result") {
                            has_tool_result = true;
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let openai_id = anthropic_id_to_openai(tool_use_id);
                            let tool_content = block
                                .get("content")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .or_else(|| {
                                    block.get("content").and_then(|v| v.as_array()).and_then(
                                        |arr| {
                                            let texts: Vec<String> = arr
                                                .iter()
                                                .filter_map(|b| {
                                                    b.get("text")
                                                        .and_then(|t| t.as_str())
                                                        .map(|s| s.to_string())
                                                })
                                                .collect();
                                            Some(texts.join(""))
                                        },
                                    )
                                })
                                .unwrap_or_default();
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": openai_id,
                                "content": tool_content
                            }));
                        } else {
                            non_tool_blocks.push(block.clone());
                        }
                    }
                    if has_tool_result {
                        messages.extend(tool_results);
                        if !non_tool_blocks.is_empty() {
                            let converted = convert_content(&Value::Array(non_tool_blocks));
                            if !converted.is_null() {
                                messages.push(json!({"role": "user", "content": converted}));
                            }
                        }
                        continue;
                    }
                }
            }

            // assistant 消息：处理 tool_use -> tool_calls
            if role == "assistant" {
                let mut text_parts = Vec::new();
                let mut thinking_parts = Vec::new();
                let mut tool_calls = Vec::new();

                if let Some(content_arr) = content.as_array() {
                    for block in content_arr {
                        let block_type = block.get("type").and_then(|v| v.as_str());
                        match block_type {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    text_parts.push(text);
                                }
                            }
                            Some("thinking") => {
                                if let Some(thinking) =
                                    block.get("thinking").and_then(|v| v.as_str())
                                {
                                    thinking_parts.push(thinking);
                                }
                            }
                            Some("tool_use") => {
                                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                let openai_id = anthropic_id_to_openai(id);
                                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let name = sanitized_tool_name(name, &original_to_sanitized);
                                let input = block.get("input").cloned().unwrap_or(json!({}));
                                tool_calls.push(json!({
                                    "id": openai_id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": input.to_string()
                                    }
                                }));
                            }
                            _ => {}
                        }
                    }
                } else if let Some(text) = content.as_str() {
                    text_parts.push(text);
                }

                let mut msg = json!({"role": "assistant"});
                if text_parts.is_empty() {
                    msg["content"] = json!(null);
                } else {
                    msg["content"] = json!(text_parts.join(""));
                }
                // 部分 provider（如 DeepSeek V4）要求 all-or-nothing：历史中有任一
                // assistant 带 reasoning_content 时，后续都必须包含（允许空字符串）。
                if quirks.reasoning_all_or_nothing || !thinking_parts.is_empty() {
                    msg["reasoning_content"] = json!(thinking_parts.join(""));
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                messages.push(msg);
            } else {
                let converted = convert_content(&content);
                messages.push(json!({"role": role, "content": converted}));
            }
        }
    }

    // output_config -> response_format
    let mut json_schema_hint = None;
    if let Some(output_config) = body.get("output_config") {
        if let Some(format_val) = output_config.get("format") {
            let format_type = format_val
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("text");
            match format_type {
                "json_schema" => {
                    if quirks.no_json_schema {
                        // 不支持 json_schema 的 provider，降级为 json_object + schema 注入
                        openai["response_format"] = json!({"type": "json_object"});
                        if let Some(schema) = format_val.get("schema") {
                            if let Ok(schema_str) = serde_json::to_string(schema) {
                                json_schema_hint = Some(format!(
                                    "You must respond with a valid JSON object that strictly conforms to the following JSON Schema. Do not include any markdown code fences, explanations, or extra text outside the JSON object.\n\nSchema:\n{}",
                                    schema_str
                                ));
                            }
                        }
                    } else {
                        openai["response_format"] = format_val.clone();
                    }
                }
                "json_object" => {
                    openai["response_format"] = json!({"type": "json_object"});
                    json_schema_hint = Some("Respond with a valid JSON object. Do not include any markdown code fences, explanations, or extra text outside the JSON object.".to_string());
                }
                other => {
                    warn!("未识别的 output_config.format.type: {}，不做转换", other);
                }
            }
        }
    }

    // 如果存在 json_schema hint，插入到 system messages 之后
    if let Some(hint) = json_schema_hint {
        let mut inserted = false;
        for (i, msg) in messages.iter().enumerate() {
            if msg.get("role").and_then(|v| v.as_str()) != Some("system") {
                messages.insert(i, json!({"role": "system", "content": hint}));
                inserted = true;
                break;
            }
        }
        if !inserted {
            messages.push(json!({"role": "system", "content": hint}));
        }
    }

    openai["messages"] = json!(messages);

    // max_tokens / max_completion_tokens
    let model_str = config.provider.model.as_str();
    if let Some(v) = body.get("max_tokens") {
        if is_openai_o_series(model_str) {
            openai["max_completion_tokens"] = v.clone();
        } else {
            openai["max_tokens"] = v.clone();
        }
    }

    // reasoning_effort
    if supports_reasoning_effort(model_str) {
        if let Some(effort) = resolve_reasoning_effort(&body, &quirks.max_reasoning_effort) {
            openai["reasoning_effort"] = json!(effort);
        }
    }

    // 透传其他参数
    for key in ["temperature", "top_p", "top_k", "stream"] {
        if let Some(v) = body.get(key) {
            openai[key] = v.clone();
        }
    }

    // stop_sequences -> stop
    if let Some(v) = body.get("stop_sequences") {
        openai["stop"] = v.clone();
    }

    // tool_choice
    if let Some(tool_choice) = body.get("tool_choice") {
        openai["tool_choice"] = convert_tool_choice(tool_choice, &original_to_sanitized);
    }

    // 流式响应时请求上游返回 usage（OpenAI 兼容格式）
    if body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        openai["stream_options"] = json!({"include_usage": true});
    }

    // thinking 参数（DeepSeek 目前自动返回 reasoning_content，暂不透传，仅记录）
    if let Some(thinking) = body.get("thinking") {
        debug!("客户端请求 thinking 配置: {}", thinking);
    }

    (openai, forward_map)
}

/// 解析 JSON body，转换格式，并判断是否流式
fn prepare_body(
    mut body: Value,
    config: &Config,
) -> Result<(Value, bool, HashMap<String, String>)> {
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match config.provider.format {
        crate::config::ProviderFormat::Openai => {
            let (openai_body, tool_name_map) = anthropic_to_openai(body, config);
            Ok((openai_body, stream, tool_name_map))
        }
        crate::config::ProviderFormat::Anthropic => {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("model".to_string(), json!(config.provider.model.clone()));
            }
            Ok((body, stream, HashMap::new()))
        }
    }
}

/// 构建转发到 Provider 的请求
fn build_provider_request(
    client: &Client,
    config: &Config,
    body: Value,
) -> reqwest::RequestBuilder {
    match config.provider.format {
        crate::config::ProviderFormat::Openai => {
            let url = format!(
                "{}/v1/chat/completions",
                config.provider.base_url.trim_end_matches('/')
            );
            client
                .post(&url)
                .header("content-type", "application/json")
                .header(
                    "authorization",
                    format!("Bearer {}", config.provider.api_key),
                )
                .json(&body)
        }
        crate::config::ProviderFormat::Anthropic => {
            let url = format!(
                "{}/v1/messages",
                config.provider.base_url.trim_end_matches('/')
            );
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-api-key", config.provider.api_key.as_str())
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        }
    }
}

/// 估算输入 token 数（粗略）
fn estimate_input_tokens(body: &Value) -> u64 {
    fn estimate_json_chars(value: &Value) -> usize {
        match value {
            Value::Null => 4,
            Value::Bool(_) => 5,
            Value::Number(n) => n.to_string().len(),
            Value::String(s) => s.len(),
            Value::Array(arr) => {
                2 + arr
                    .iter()
                    .map(estimate_json_chars)
                    .sum::<usize>()
                    .saturating_add(arr.len().saturating_sub(1))
            }
            Value::Object(obj) => {
                2 + obj
                    .iter()
                    .map(|(key, value)| key.len() + estimate_json_chars(value) + 3)
                    .sum::<usize>()
                    .saturating_add(obj.len().saturating_sub(1))
            }
        }
    }

    // 统计 messages 中所有 content 的字符数（包括 text/thinking/tool_result/tool_use）
    let msg_chars: usize = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .map(|msg| {
                    msg.get("content")
                        .map(|c| match c {
                            Value::String(s) => s.len(),
                            Value::Array(a) => a
                                .iter()
                                .map(|item| {
                                    let mut chars = 0usize;
                                    // text block
                                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                        chars += text.len();
                                    }
                                    // thinking block
                                    if let Some(thinking) =
                                        item.get("thinking").and_then(|t| t.as_str())
                                    {
                                        chars += thinking.len();
                                    }
                                    // tool_result content (string or array of text blocks)
                                    if let Some(content) = item.get("content") {
                                        if let Some(s) = content.as_str() {
                                            chars += s.len();
                                        } else if let Some(arr) = content.as_array() {
                                            chars += arr
                                                .iter()
                                                .filter_map(|b| {
                                                    b.get("text")
                                                        .and_then(|t| t.as_str())
                                                        .map(|s| s.len())
                                                })
                                                .sum::<usize>();
                                        }
                                    }
                                    // tool_use name + input
                                    if let Some(name) = item.get("name").and_then(|t| t.as_str()) {
                                        chars += name.len();
                                    }
                                    if let Some(input) = item.get("input") {
                                        chars += estimate_json_chars(input);
                                    }
                                    chars
                                })
                                .sum(),
                            _ => 0,
                        })
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0);

    // system 提示
    let system_chars: usize = body
        .get("system")
        .map(|s| match s {
            Value::String(text) => text.len(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(|s| s.len()))
                .sum(),
            _ => 0,
        })
        .unwrap_or(0);

    // tools 定义（粗略估算）
    let tools_chars: usize = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| arr.iter().map(estimate_json_chars).sum())
        .unwrap_or(0);

    // 粗略估算：代码/JSON 密度高，用 /3 比 /4 更接近实际（自然语言约 /4，代码约 /2~3）
    ((msg_chars + system_chars + tools_chars) / 3) as u64
}

/// 处理 Anthropic 消息请求
pub async fn proxy_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let request_id = next_request_id();
    let request_start = Instant::now();
    let mut request_guard = RequestCompletionGuard::new(request_id.clone(), request_start);

    // 1. 认证检查
    request_guard.set_phase("auth");
    check_auth(&headers, &state.config)?;

    // 2. 读取请求体
    request_guard.set_phase("read_body");
    let bytes = to_bytes(body, state.config.server.max_body_bytes)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("length limit") || msg.contains("LengthLimit") {
                AppError::PayloadTooLarge
            } else {
                AppError::Request(format!("读取请求体失败: {}", msg))
            }
        })?;

    let body_json: Value = serde_json::from_slice(&bytes)?;
    request_guard.set_phase("received_body");
    info!(
        request_id = request_id.as_str(),
        body_bytes = bytes.len(),
        body_limit_bytes = state.config.server.max_body_bytes,
        model = body_json
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        stream = body_json
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        messages = message_count(&body_json),
        tools = tool_count(&body_json),
        "收到客户端请求"
    );

    let input_tokens = estimate_input_tokens(&body_json);

    request_guard.set_phase("prepare_body");
    let (body_json, is_stream, tool_name_map) = prepare_body(body_json, &state.config)?;

    info!(
        request_id = request_id.as_str(),
        provider_model = state.config.provider.model.as_str(),
        provider_format = ?state.config.provider.format,
        stream = is_stream,
        messages = message_count(&body_json),
        tools = tool_count(&body_json),
        "准备转发上游请求"
    );

    // 3. 发送请求到 Provider
    let req = build_provider_request(&state.client, &state.config, body_json);

    let upstream_start = Instant::now();
    request_guard.set_phase("send_upstream");
    let upstream_resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            error!(
                request_id = request_id.as_str(),
                error = %e,
                upstream_total_ms = elapsed_ms(upstream_start),
                request_total_ms = elapsed_ms(request_start),
                "上游请求发送失败"
            );
            request_guard.complete();
            return Err(AppError::Http(e));
        }
    };
    let upstream_headers_ms = elapsed_ms(upstream_start);
    let status = upstream_resp.status();
    request_guard.set_phase("received_upstream_headers");

    info!(
        request_id = request_id.as_str(),
        status = %status,
        upstream_headers_ms,
        "收到上游响应头"
    );

    if !status.is_success() {
        let text = upstream_resp.text().await.unwrap_or_default();
        let log_body = truncate_for_log(&text, MAX_LOG_BODY_BYTES);
        error!(
            request_id = request_id.as_str(),
            status = %status,
            body_bytes = text.len(),
            body = %log_body,
            upstream_total_ms = elapsed_ms(upstream_start),
            request_total_ms = elapsed_ms(request_start),
            "上游返回错误"
        );
        let response = (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(json!({
                "type": "error",
                "error": {
                    "type": "upstream_error",
                    "message": text
                }
            })),
        )
            .into_response();
        request_guard.complete();
        return Ok(response);
    }

    let model = state.config.provider.model.clone();

    // 4. 根据是否流式返回不同响应
    request_guard.set_phase(if is_stream {
        "handle_stream_response"
    } else {
        "handle_non_stream_response"
    });
    let response = match state.config.provider.format {
        crate::config::ProviderFormat::Openai => {
            if is_stream {
                handle_stream(
                    upstream_resp,
                    &model,
                    input_tokens,
                    &tool_name_map,
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            } else {
                handle_non_stream(
                    upstream_resp,
                    &model,
                    &tool_name_map,
                    &request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
        crate::config::ProviderFormat::Anthropic => {
            if is_stream {
                handle_stream_passthrough(
                    upstream_resp,
                    request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            } else {
                handle_non_stream_passthrough(
                    upstream_resp,
                    &request_id,
                    request_start,
                    upstream_start,
                    upstream_headers_ms,
                )
                .await
            }
        }
    };
    request_guard.complete();
    response
}

// ==================== 非流式响应转换 ====================

async fn handle_non_stream(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_reverse_map: &HashMap<String, String>,
    request_id: &str,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
) -> Result<Response> {
    let content_type = upstream_resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let is_json = media_type == "application/json" || media_type.ends_with("+json");

    let body_text = upstream_resp.text().await?;
    let body: Value = serde_json::from_str(&body_text).map_err(|e| {
        let preview = truncate_for_log(&body_text, MAX_LOG_BODY_BYTES);
        if is_json {
            error!(
                request_id,
                content_type = %content_type,
                body_preview = %preview,
                error = %e,
                "上游返回了 Content-Type: application/json 但 JSON 解析失败"
            );
        } else {
            error!(
                request_id,
                content_type = %content_type,
                body_preview = %preview,
                "上游返回了非 JSON 响应"
            );
        }
        AppError::UpstreamInvalidResponse(format!(
            "上游响应格式异常: content-type={}, 解析失败: {}",
            content_type, e
        ))
    })?;
    info!(
        request_id,
        response_id = body.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        choices = body
            .get("choices")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len())
            .unwrap_or(0),
        has_usage = body.get("usage").is_some(),
        upstream_headers_ms,
        upstream_total_ms = elapsed_ms(upstream_start),
        request_total_ms = elapsed_ms(request_start),
        "上游非流式响应"
    );

    let anthropic = convert_non_stream_response(body, model, tool_name_reverse_map).await;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(anthropic.to_string()))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

async fn convert_non_stream_response(
    body: Value,
    model: &str,
    tool_name_reverse_map: &HashMap<String, String>,
) -> Value {
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("msg_00000000000000000000")
        .to_string();

    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .cloned()
        .unwrap_or(json!({}));

    let message = choice.get("message").cloned().unwrap_or(json!({}));

    let mut content_blocks = Vec::new();
    let mut has_tool_use = false;

    // reasoning_content -> thinking
    if let Some(reasoning) = message.get("reasoning_content").and_then(|v| v.as_str()) {
        if !reasoning.is_empty() {
            content_blocks.push(json!({
                "type": "thinking",
                "thinking": reasoning,
                "signature": ""
            }));
        }
    }

    // text content / refusal
    if let Some(msg_content) = message.get("content") {
        if let Some(text) = msg_content.as_str() {
            if !text.is_empty() {
                content_blocks.push(json!({
                    "type": "text",
                    "text": text
                }));
            }
        } else if let Some(parts) = msg_content.as_array() {
            for part in parts {
                let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match part_type {
                    "text" | "output_text" => {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                content_blocks.push(json!({"type": "text", "text": text}));
                            }
                        }
                    }
                    "refusal" => {
                        if let Some(refusal) = part.get("refusal").and_then(|r| r.as_str()) {
                            if !refusal.is_empty() {
                                content_blocks.push(json!({"type": "text", "text": refusal}));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Some providers put refusal at message-level
    if let Some(refusal) = message.get("refusal").and_then(|r| r.as_str()) {
        if !refusal.is_empty() {
            content_blocks.push(json!({"type": "text", "text": refusal}));
        }
    }

    // tool_calls -> tool_use
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        if !tool_calls.is_empty() {
            has_tool_use = true;
        }
        for call in tool_calls {
            let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let function = call.get("function").cloned().unwrap_or(json!({}));
            let name = function.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let original_name = tool_name_reverse_map
                .get(name)
                .map(|s| s.as_str())
                .unwrap_or(name);
            let arguments = function
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));

            let anthropic_id = openai_id_to_anthropic(call_id);

            content_blocks.push(json!({
                "type": "tool_use",
                "id": anthropic_id,
                "name": original_name,
                "input": args
            }));
        }
    }

    // 兼容旧格式 function_call
    if !has_tool_use {
        if let Some(function_call) = message.get("function_call") {
            let id = function_call
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("");
            let name = function_call
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let has_arguments = function_call.get("arguments").is_some();
            let input = match function_call.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!({})),
                Some(v @ Value::Object(_)) | Some(v @ Value::Array(_)) => v.clone(),
                _ => json!({}),
            };
            if !name.is_empty() || has_arguments {
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": openai_id_to_anthropic(id),
                    "name": name,
                    "input": input
                }));
                has_tool_use = true;
            }
        }
    }

    let finish_reason = choice.get("finish_reason").and_then(|v| v.as_str());

    let stop_reason = match finish_reason {
        Some("stop") => "end_turn",
        Some("length") => "max_tokens",
        Some("content_filter") => "end_turn",
        Some("tool_calls") | Some("function_call") => "tool_use",
        _ => {
            if has_tool_use {
                "tool_use"
            } else {
                "end_turn"
            }
        }
    };

    let usage = body.get("usage").cloned().unwrap_or(json!({}));
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = usage
        .get("prompt_cache_hit_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation = usage
        .get("prompt_cache_miss_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    json!({
        "type": "message",
        "id": id,
        "role": "assistant",
        "content": content_blocks,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_read_input_tokens": cache_read,
            "cache_creation_input_tokens": cache_creation
        }
    })
}

// ==================== 流式响应转换 ====================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamBlockKind {
    Thinking,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CurrentBlock {
    kind: StreamBlockKind,
    index: usize,
}

fn next_stream_block_index(next_content_block_index: &mut usize) -> usize {
    let index = *next_content_block_index;
    *next_content_block_index += 1;
    index
}

fn push_content_block_stop(events: &mut Vec<String>, index: usize) {
    events.push(format!(
        "event: content_block_stop\ndata: {}\n\n",
        json!({"type": "content_block_stop", "index": index})
    ));
}

fn stop_current_stream_block(events: &mut Vec<String>, current_block: &mut Option<CurrentBlock>) {
    if let Some(block) = current_block.take() {
        push_content_block_stop(events, block.index);
    }
}

fn close_open_tool_blocks(events: &mut Vec<String>, open_tool_blocks: &mut BTreeSet<usize>) {
    for index in std::mem::take(open_tool_blocks) {
        push_content_block_stop(events, index);
    }
}

async fn handle_stream(
    upstream_resp: reqwest::Response,
    model: &str,
    input_tokens: u64,
    tool_name_reverse_map: &HashMap<String, String>,
    request_id: String,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
) -> Result<Response> {
    info!(
        request_id = request_id.as_str(),
        upstream_headers_ms, "开始处理上游流式响应"
    );
    let byte_stream = upstream_resp.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<Bytes, AppError>>(128);

    let model = model.to_string();
    let reverse_map = tool_name_reverse_map.clone();

    tokio::spawn(async move {
        let stream_start = Instant::now();
        let mut buffer = String::new();
        let mut utf8_remainder: Vec<u8> = Vec::new();
        let mut stream_id = String::new();
        let mut started = false;
        let mut ended = false;
        let mut current_block: Option<CurrentBlock> = None;
        let mut next_content_block_index: usize = 0;
        let mut output_tokens: usize = 0;
        let mut pending_message_delta: Option<String> = None;
        let mut has_emitted_message_delta = false;
        let mut tool_block_indices: HashMap<usize, usize> = HashMap::new();
        let mut open_tool_blocks: BTreeSet<usize> = BTreeSet::new();
        let mut actual_output_tokens: Option<u64> = None;
        let mut actual_input_tokens: Option<u64> = None;
        let mut stop_reason_value: Option<String> = None;
        let mut upstream_chunks: u64 = 0;
        let mut emitted_events: u64 = 0;

        let log_stream_end = |reason: &str,
                              stream_id: &str,
                              started: bool,
                              ended: bool,
                              upstream_chunks: u64,
                              emitted_events: u64,
                              actual_input_tokens: Option<u64>,
                              actual_output_tokens: Option<u64>,
                              stop_reason_value: Option<&str>| {
            info!(
                request_id = request_id.as_str(),
                stream_id,
                reason,
                started,
                ended,
                upstream_chunks,
                emitted_events,
                actual_input_tokens,
                actual_output_tokens,
                stop_reason = stop_reason_value.unwrap_or(""),
                upstream_headers_ms,
                upstream_total_ms = elapsed_ms(upstream_start),
                stream_total_ms = elapsed_ms(stream_start),
                request_total_ms = elapsed_ms(request_start),
                "流式响应结束"
            );
        };

        let mut stream = byte_stream;

        while let Some(result) = stream.next().await {
            match result {
                Ok(bytes) => {
                    append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);

                    loop {
                        if let Some((pos, delimiter_len)) = find_sse_block_end(&buffer) {
                            let block = buffer[..pos].to_string();
                            buffer.drain(..pos + delimiter_len);

                            for line in block.lines() {
                                if !line.starts_with("data: ") {
                                    continue;
                                }
                                let data = &line[6..];

                                if data == "[DONE]" {
                                    upstream_chunks += 1;
                                    // 如果上游返回了实际 usage，用实际值重新生成 message_delta
                                    if let Some(actual) = actual_output_tokens {
                                        let sr = stop_reason_value.as_deref().unwrap_or("end_turn");
                                        let mut usage = json!({"output_tokens": actual});
                                        // 上游若返回了 prompt_tokens，一并透传
                                        if let Some(prompt) = actual_input_tokens {
                                            usage["input_tokens"] = json!(prompt);
                                        }
                                        let delta = format!(
                                            "event: message_delta\ndata: {}\n\n",
                                            json!({
                                                "type": "message_delta",
                                                "delta": {
                                                    "stop_reason": sr,
                                                    "stop_sequence": null
                                                },
                                                "usage": usage
                                            })
                                        );
                                        match tx.send(Ok(Bytes::from(delta))).await {
                                            Ok(_) => emitted_events += 1,
                                            Err(_) => {
                                                log_stream_end(
                                                    "client_disconnected",
                                                    &stream_id,
                                                    started,
                                                    ended,
                                                    upstream_chunks,
                                                    emitted_events,
                                                    actual_input_tokens,
                                                    actual_output_tokens,
                                                    stop_reason_value.as_deref(),
                                                );
                                                return;
                                            }
                                        }
                                    } else if let Some(delta) = pending_message_delta.take() {
                                        match tx.send(Ok(Bytes::from(delta))).await {
                                            Ok(_) => emitted_events += 1,
                                            Err(_) => {
                                                log_stream_end(
                                                    "client_disconnected",
                                                    &stream_id,
                                                    started,
                                                    ended,
                                                    upstream_chunks,
                                                    emitted_events,
                                                    actual_input_tokens,
                                                    actual_output_tokens,
                                                    stop_reason_value.as_deref(),
                                                );
                                                return;
                                            }
                                        }
                                    }
                                    if send_message_stop(&tx).await {
                                        emitted_events += 1;
                                        log_stream_end(
                                            "done",
                                            &stream_id,
                                            started,
                                            ended,
                                            upstream_chunks,
                                            emitted_events,
                                            actual_input_tokens,
                                            actual_output_tokens,
                                            stop_reason_value.as_deref(),
                                        );
                                    } else {
                                        log_stream_end(
                                            "client_disconnected",
                                            &stream_id,
                                            started,
                                            ended,
                                            upstream_chunks,
                                            emitted_events,
                                            actual_input_tokens,
                                            actual_output_tokens,
                                            stop_reason_value.as_deref(),
                                        );
                                    }
                                    return;
                                }

                                if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                                    upstream_chunks += 1;
                                    // 检查上游是否在最后一个 chunk 返回了 usage
                                    if let Some(usage) = chunk.get("usage") {
                                        if let Some(completion) =
                                            usage.get("completion_tokens").and_then(|v| v.as_u64())
                                        {
                                            actual_output_tokens = Some(completion);
                                        }
                                        if let Some(prompt) =
                                            usage.get("prompt_tokens").and_then(|v| v.as_u64())
                                        {
                                            actual_input_tokens = Some(prompt);
                                        }
                                        // 如果 choices 为空，这是纯 usage chunk，跳过
                                        if chunk
                                            .get("choices")
                                            .and_then(|c| c.as_array())
                                            .map(|arr| arr.is_empty())
                                            .unwrap_or(false)
                                        {
                                            continue;
                                        }
                                    }

                                    let events = convert_stream_chunk(
                                        &chunk,
                                        &model,
                                        &mut stream_id,
                                        &mut started,
                                        &mut current_block,
                                        &mut next_content_block_index,
                                        &mut ended,
                                        &mut pending_message_delta,
                                        input_tokens,
                                        &mut output_tokens,
                                        &reverse_map,
                                        &mut tool_block_indices,
                                        &mut open_tool_blocks,
                                        &mut stop_reason_value,
                                    );

                                    for event in events {
                                        match tx.send(Ok(Bytes::from(event))).await {
                                            Ok(_) => emitted_events += 1,
                                            Err(_) => {
                                                log_stream_end(
                                                    "client_disconnected",
                                                    &stream_id,
                                                    started,
                                                    ended,
                                                    upstream_chunks,
                                                    emitted_events,
                                                    actual_input_tokens,
                                                    actual_output_tokens,
                                                    stop_reason_value.as_deref(),
                                                );
                                                return;
                                            }
                                        }
                                    }

                                    if ended {
                                        has_emitted_message_delta = true;
                                    }
                                }
                            }
                        } else {
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!(
                        request_id = request_id.as_str(),
                        error = %e,
                        upstream_headers_ms,
                        upstream_total_ms = elapsed_ms(upstream_start),
                        stream_total_ms = elapsed_ms(stream_start),
                        request_total_ms = elapsed_ms(request_start),
                        "上游流式读取错误"
                    );
                    let _ = tx.send(Err(AppError::Http(e))).await;
                    log_stream_end(
                        "upstream_error",
                        &stream_id,
                        started,
                        ended,
                        upstream_chunks,
                        emitted_events,
                        actual_input_tokens,
                        actual_output_tokens,
                        stop_reason_value.as_deref(),
                    );
                    return;
                }
            }
        }

        // 流结束但未收到 [DONE]
        if started && !ended {
            emitted_events += send_final_events(&tx, &current_block, &open_tool_blocks).await;
            if send_message_stop(&tx).await {
                emitted_events += 1;
            }
            log_stream_end(
                "eof_without_done",
                &stream_id,
                started,
                ended,
                upstream_chunks,
                emitted_events,
                actual_input_tokens,
                actual_output_tokens,
                stop_reason_value.as_deref(),
            );
        } else if started && ended {
            // 流已结束（有 finish_reason），但未收到 [DONE]，补发 message_stop
            if !has_emitted_message_delta {
                if let Some(delta) = pending_message_delta.take() {
                    if tx.send(Ok(Bytes::from(delta))).await.is_ok() {
                        emitted_events += 1;
                    }
                }
                if send_message_stop(&tx).await {
                    emitted_events += 1;
                }
            }
            log_stream_end(
                "eof_after_finish",
                &stream_id,
                started,
                ended,
                upstream_chunks,
                emitted_events,
                actual_input_tokens,
                actual_output_tokens,
                stop_reason_value.as_deref(),
            );
        } else {
            log_stream_end(
                "eof_before_start",
                &stream_id,
                started,
                ended,
                upstream_chunks,
                emitted_events,
                actual_input_tokens,
                actual_output_tokens,
                stop_reason_value.as_deref(),
            );
        }
    });

    let body_stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(Ok(bytes)) => Some((Ok::<Bytes, AppError>(bytes), rx)),
            Some(Err(e)) => Some((Err::<Bytes, _>(e), rx)),
            None => None,
        }
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body_stream))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

async fn send_final_events(
    tx: &tokio::sync::mpsc::Sender<std::result::Result<Bytes, AppError>>,
    current_block: &Option<CurrentBlock>,
    open_tool_blocks: &BTreeSet<usize>,
) -> u64 {
    let mut sent = 0;
    if let Some(block) = *current_block {
        if tx
            .send(Ok(Bytes::from(format!(
                "event: content_block_stop\ndata: {}\n\n",
                json!({"type": "content_block_stop", "index": block.index})
            ))))
            .await
            .is_ok()
        {
            sent += 1;
        }
    }

    for index in open_tool_blocks {
        if tx
            .send(Ok(Bytes::from(format!(
                "event: content_block_stop\ndata: {}\n\n",
                json!({"type": "content_block_stop", "index": index})
            ))))
            .await
            .is_ok()
        {
            sent += 1;
        }
    }

    sent
}

async fn send_message_stop(
    tx: &tokio::sync::mpsc::Sender<std::result::Result<Bytes, AppError>>,
) -> bool {
    let msg_stop = format!(
        "event: message_stop\ndata: {}\n\n",
        json!({"type": "message_stop"})
    );
    tx.send(Ok(Bytes::from(msg_stop))).await.is_ok()
}

fn convert_stream_chunk(
    chunk: &Value,
    model: &str,
    stream_id: &mut String,
    started: &mut bool,
    current_block: &mut Option<CurrentBlock>,
    next_content_block_index: &mut usize,
    ended: &mut bool,
    pending_message_delta: &mut Option<String>,
    input_tokens: u64,
    output_tokens: &mut usize,
    tool_name_reverse_map: &HashMap<String, String>,
    tool_block_indices: &mut HashMap<usize, usize>,
    open_tool_blocks: &mut BTreeSet<usize>,
    stop_reason_value: &mut Option<String>,
) -> Vec<String> {
    let mut events = Vec::new();

    let id = chunk
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if !id.is_empty() && stream_id.is_empty() {
        *stream_id = id;
    }

    let choice = chunk
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .cloned()
        .unwrap_or(json!({}));

    let delta = choice.get("delta").cloned().unwrap_or(json!({}));
    let content = delta.get("content").and_then(|v| v.as_str());
    let reasoning = delta.get("reasoning_content").and_then(|v| v.as_str());
    let finish_reason = choice.get("finish_reason").and_then(|v| v.as_str());

    // 第一个 chunk（有 role）
    if !*started && delta.get("role").is_some() {
        *started = true;
        events.push(format!(
            "event: message_start\ndata: {}\n\n",
            json!({
                "type": "message_start",
                "message": {
                    "id": stream_id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": 0
                    }
                }
            })
        ));
    }

    // reasoning content -> thinking block
    if let Some(text) = reasoning {
        if !text.is_empty() {
            close_open_tool_blocks(&mut events, open_tool_blocks);
            if current_block.map(|block| block.kind) != Some(StreamBlockKind::Thinking) {
                stop_current_stream_block(&mut events, current_block);
                let index = next_stream_block_index(next_content_block_index);
                events.push(format!(
                    "event: content_block_start\ndata: {}\n\n",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "thinking",
                            "thinking": ""
                        }
                    })
                ));
                *current_block = Some(CurrentBlock {
                    kind: StreamBlockKind::Thinking,
                    index,
                });
            }
            let index = current_block.map(|block| block.index).unwrap_or(0);
            events.push(format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "thinking_delta",
                        "thinking": text
                    }
                })
            ));
            *output_tokens += text.len() / 4 + 1;
        }
    }

    // text content -> text block
    if let Some(text) = content {
        if !text.is_empty() {
            close_open_tool_blocks(&mut events, open_tool_blocks);
            if current_block.map(|block| block.kind) != Some(StreamBlockKind::Text) {
                stop_current_stream_block(&mut events, current_block);
                let index = next_stream_block_index(next_content_block_index);
                events.push(format!(
                    "event: content_block_start\ndata: {}\n\n",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "text",
                            "text": ""
                        }
                    })
                ));
                *current_block = Some(CurrentBlock {
                    kind: StreamBlockKind::Text,
                    index,
                });
            }
            let index = current_block.map(|block| block.index).unwrap_or(0);
            events.push(format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "text_delta",
                        "text": text
                    }
                })
            ));
            *output_tokens += text.len() / 4 + 1;
        }
    }

    // tool_calls -> tool_use blocks
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            if let Some(index_val) = call.get("index").and_then(|v| v.as_u64()) {
                let index = index_val as usize;
                let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let function = call.get("function").cloned().unwrap_or(json!({}));
                let name = function.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = function
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if !name.is_empty() && !tool_block_indices.contains_key(&index) {
                    stop_current_stream_block(&mut events, current_block);
                    let block_index = next_stream_block_index(next_content_block_index);
                    let original_name = tool_name_reverse_map
                        .get(name)
                        .map(|s| s.as_str())
                        .unwrap_or(name);
                    let anthropic_id = openai_id_to_anthropic(call_id);
                    events.push(format!(
                        "event: content_block_start
data: {}

",
                        json!({
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": anthropic_id,
                                "name": original_name,
                                "input": {}
                            }
                        })
                    ));
                    tool_block_indices.insert(index, block_index);
                    open_tool_blocks.insert(block_index);
                }

                if !arguments.is_empty() {
                    let block_index = tool_block_indices.get(&index).copied().unwrap_or(index);
                    events.push(format!(
                        "event: content_block_delta
data: {}

",
                        json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": arguments
                            }
                        })
                    ));
                }
            }
        }
    }

    // finish
    if finish_reason.is_some() {
        *ended = true;
        stop_current_stream_block(&mut events, current_block);
        close_open_tool_blocks(&mut events, open_tool_blocks);
        let stop_reason = match finish_reason {
            Some("stop") => "end_turn",
            Some("length") => "max_tokens",
            Some("content_filter") => "end_turn",
            Some("tool_calls") | Some("function_call") => "tool_use",
            _ => "end_turn",
        };
        *stop_reason_value = Some(stop_reason.to_string());
        // 缓存 message_delta，等到 [DONE] 再发（output_tokens 可能被上游实际值覆盖）
        *pending_message_delta = Some(format!(
            "event: message_delta\ndata: {}\n\n",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": {
                    "output_tokens": *output_tokens
                }
            })
        ));
    }

    events
}

/// Claude Code 遥测事件日志 — 记录摘要到 debug 日志
pub async fn event_logging_batch(body: String) -> Json<Value> {
    let body_len = body.len();
    let body_preview = truncate_for_log(&body, MAX_LOG_BODY_BYTES);
    // 尝试提取事件数量
    let event_count = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.as_array().map(|arr| arr.len()))
        .unwrap_or(0);
    debug!(
        event_count,
        body_bytes = body_len,
        body = %body_preview,
        "收到遥测事件"
    );
    Json(json!({"status": "ok"}))
}

// ==================== Anthropic 格式透传 ====================

async fn handle_non_stream_passthrough(
    upstream_resp: reqwest::Response,
    request_id: &str,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
) -> Result<Response> {
    let bytes = upstream_resp.bytes().await?;
    info!(
        request_id,
        body_bytes = bytes.len(),
        upstream_headers_ms,
        upstream_total_ms = elapsed_ms(upstream_start),
        request_total_ms = elapsed_ms(request_start),
        "上游非流式透传响应"
    );
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

async fn handle_stream_passthrough(
    upstream_resp: reqwest::Response,
    request_id: String,
    request_start: Instant,
    upstream_start: Instant,
    upstream_headers_ms: u128,
) -> Result<Response> {
    let byte_stream = upstream_resp.bytes_stream();
    let stream_start = Instant::now();
    let mut chunk_count: u64 = 0;
    let body_stream = byte_stream.map(move |result| match result {
        Ok(bytes) => {
            chunk_count += 1;
            Ok(bytes)
        }
        Err(e) => {
            error!(
                request_id = request_id.as_str(),
                error = %e,
                upstream_headers_ms,
                upstream_total_ms = elapsed_ms(upstream_start),
                stream_total_ms = elapsed_ms(stream_start),
                request_total_ms = elapsed_ms(request_start),
                chunks = chunk_count,
                "上游流式透传读取错误"
            );
            Err(std::io::Error::new(std::io::ErrorKind::Other, e))
        }
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body_stream))
        .map_err(|e| AppError::Request(format!("构建响应失败: {}", e)))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        test_config_with_model("deepseek-v4-pro")
    }

    fn test_config_with_model(model: &str) -> Config {
        Config {
            server: crate::config::ServerConfig {
                port: 4000,
                api_key: None,
                max_body_bytes: crate::config::DEFAULT_MAX_BODY_BYTES,
            },
            provider: crate::config::ProviderConfig {
                base_url: "http://127.0.0.1:9999".to_string(),
                api_key: "test-provider-key".to_string(),
                model: model.to_string(),
                format: crate::config::ProviderFormat::Openai,
                quirks: Default::default(),
            },
        }
    }

    #[test]
    fn tool_choice_uses_sanitized_tool_name() {
        let body = json!({
            "model": "claude-test",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "tool.search",
                "description": "Search",
                "input_schema": {
                    "type": "object",
                    "properties": {}
                }
            }],
            "tool_choice": {
                "type": "tool",
                "name": "tool.search"
            }
        });

        let (openai, tool_name_map) = anthropic_to_openai(body, &test_config());

        assert_eq!(
            openai["tools"][0]["function"]["name"].as_str(),
            Some("tool_search")
        );
        assert_eq!(
            openai["tool_choice"]["function"]["name"].as_str(),
            Some("tool_search")
        );
        assert_eq!(
            tool_name_map.get("tool_search").map(String::as_str),
            Some("tool.search")
        );
    }

    #[test]
    fn historical_tool_use_uses_sanitized_tool_name() {
        let body = json!({
            "model": "claude-test",
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": "tool.search",
                    "input": {"query": "rust"}
                }]
            }],
            "tools": [{
                "name": "tool.search",
                "description": "Search",
                "input_schema": {
                    "type": "object",
                    "properties": {}
                }
            }]
        });

        let (openai, _) = anthropic_to_openai(body, &test_config());

        assert_eq!(
            openai["messages"][0]["tool_calls"][0]["function"]["name"].as_str(),
            Some("tool_search")
        );
    }

    #[test]
    fn mixed_tool_result_preserves_user_text() {
        let body = json!({
            "model": "claude-test",
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_abc",
                        "content": "result text"
                    },
                    {
                        "type": "text",
                        "text": "please continue"
                    }
                ]
            }]
        });

        let (openai, _) = anthropic_to_openai(body, &test_config());

        assert_eq!(openai["messages"][0]["role"].as_str(), Some("tool"));
        assert_eq!(
            openai["messages"][0]["tool_call_id"].as_str(),
            Some("call_abc")
        );
        assert_eq!(
            openai["messages"][0]["content"].as_str(),
            Some("result text")
        );
        assert_eq!(openai["messages"][1]["role"].as_str(), Some("user"));
        assert_eq!(
            openai["messages"][1]["content"].as_str(),
            Some("please continue")
        );
    }

    #[test]
    fn max_tokens_uses_provider_model_capabilities() {
        let body = json!({
            "model": "o1-mini",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024
        });

        let (openai, _) = anthropic_to_openai(body, &test_config());

        assert_eq!(openai["max_tokens"].as_u64(), Some(1024));
        assert!(openai.get("max_completion_tokens").is_none());

        let body = json!({
            "model": "claude-test",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024
        });
        let (openai, _) = anthropic_to_openai(body, &test_config_with_model("o1-mini"));

        assert_eq!(openai["max_completion_tokens"].as_u64(), Some(1024));
        assert!(openai.get("max_tokens").is_none());
    }

    #[test]
    fn sse_block_end_supports_lf_and_crlf() {
        assert_eq!(find_sse_block_end("data: {}\n\nrest"), Some((8, 2)));
        assert_eq!(find_sse_block_end("data: {}\r\n\r\nrest"), Some((8, 4)));
    }

    #[test]
    fn stream_tool_use_stop_uses_allocated_block_index() {
        let mut stream_id = String::new();
        let mut started = false;
        let mut current_block = None;
        let mut next_content_block_index = 0;
        let mut ended = false;
        let mut pending_message_delta = None;
        let mut output_tokens = 0;
        let mut tool_block_indices = HashMap::new();
        let mut open_tool_blocks = BTreeSet::new();
        let mut stop_reason_value = None;
        let tool_name_map = HashMap::new();

        let text_chunk = json!({
            "id": "chatcmpl_123",
            "choices": [{
                "delta": {
                    "role": "assistant",
                    "content": "I'll call a tool."
                },
                "finish_reason": null
            }]
        });
        let text_events = convert_stream_chunk(
            &text_chunk,
            "deepseek-v4-pro",
            &mut stream_id,
            &mut started,
            &mut current_block,
            &mut next_content_block_index,
            &mut ended,
            &mut pending_message_delta,
            10,
            &mut output_tokens,
            &tool_name_map,
            &mut tool_block_indices,
            &mut open_tool_blocks,
            &mut stop_reason_value,
        );
        let text = text_events.join("");
        assert!(text.contains("event: content_block_start"));
        assert!(text.contains(r#""index":0"#));

        let tool_chunk = json!({
            "id": "chatcmpl_123",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": "{\"query\":\"rust\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let tool_events = convert_stream_chunk(
            &tool_chunk,
            "deepseek-v4-pro",
            &mut stream_id,
            &mut started,
            &mut current_block,
            &mut next_content_block_index,
            &mut ended,
            &mut pending_message_delta,
            10,
            &mut output_tokens,
            &tool_name_map,
            &mut tool_block_indices,
            &mut open_tool_blocks,
            &mut stop_reason_value,
        );
        let text = tool_events.join("");
        assert!(text.contains("event: content_block_stop"));
        assert!(text.contains("event: content_block_start"));
        assert!(text.contains("event: content_block_delta"));
        assert!(text.contains(r#""index":0"#));
        assert!(text.contains(r#""index":1"#));
    }

    #[tokio::test]
    async fn non_stream_response_restores_original_tool_name() {
        let body = json!({
            "id": "chatcmpl_123",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "tool_search",
                            "arguments": "{\"query\":\"rust\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 3
            }
        });
        let tool_name_map = HashMap::from([("tool_search".to_string(), "tool.search".to_string())]);

        let anthropic = convert_non_stream_response(body, "deepseek-v4-pro", &tool_name_map).await;

        assert_eq!(anthropic["stop_reason"].as_str(), Some("tool_use"));
        assert_eq!(
            anthropic["content"][0]["name"].as_str(),
            Some("tool.search")
        );
        assert_eq!(
            anthropic["content"][0]["input"]["query"].as_str(),
            Some("rust")
        );
    }
}
