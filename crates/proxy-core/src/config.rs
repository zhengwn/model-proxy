use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use tracing::{info, warn};

use crate::logging::LogConfig;

pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PROVIDERS: usize = 20;
pub const MAX_NAME_LENGTH: usize = 64;

/// 配置验证错误类型
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Duplicate provider name: {0}")]
    DuplicateName(String),
    #[error("Provider not found: {0}")]
    ProviderNotFound(String),
    #[error("Provider '{provider}' missing required field: {field}")]
    MissingField { provider: String, field: String },
    #[error("No providers defined")]
    NoProviders,
    #[error("Invalid provider name: {reason}")]
    InvalidName { reason: String },
    #[error("Too many providers (max {max})")]
    TooManyProviders { max: usize },
    #[error("Cannot delete the active provider")]
    CannotDeleteActive,
    #[error("Cannot delete the last provider")]
    CannotDeleteLast,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML serialization error: {0}")]
    Toml(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// 当前活跃的 Provider（保留旧字段以兼容反序列化旧格式配置文件）
    /// 序列化时跳过此字段，避免泄漏到前端或新格式配置文件中。
    #[serde(default = "ProviderConfig::placeholder", skip_serializing)]
    pub provider: ProviderConfig,
    /// 新格式：活跃 Provider 名称
    #[serde(default)]
    pub active_provider: Option<String>,
    /// 新格式：多 Provider 列表
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// 全局模型路由规则（独立于 Provider）
    #[serde(default)]
    pub model_routes: Vec<ModelRoute>,
    /// 是否启用全局模型路由（默认 true）
    #[serde(default = "default_model_routes_enabled")]
    pub model_routes_enabled: bool,
    /// 日志配置，缺省时使用默认值
    #[serde(default)]
    pub logging: LogConfig,
    /// Fallback 配置：当活跃 Provider 失败时自动尝试备用 Provider
    #[serde(default)]
    pub fallback: FallbackConfig,
}

/// Fallback 策略配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackConfig {
    /// 是否启用自动 Fallback
    #[serde(default)]
    pub enabled: bool,
    /// 触发 Fallback 的 HTTP 状态码列表（默认：429, 500, 502, 503）
    #[serde(default = "default_fallback_status_codes")]
    pub on_status_codes: Vec<u16>,
    /// Fallback 链中尝试的最大 Provider 数量（默认 3）
    #[serde(default = "default_max_fallback_attempts")]
    pub max_attempts: usize,
}

fn default_model_routes_enabled() -> bool {
    true
}

fn default_fallback_status_codes() -> Vec<u16> {
    vec![429, 500, 502, 503]
}

fn default_max_fallback_attempts() -> usize {
    3
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on_status_codes: default_fallback_status_codes(),
            max_attempts: default_max_fallback_attempts(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub port: u16,
    /// 监听地址。默认 `127.0.0.1`（仅本机访问，安全默认值）。
    /// 如需对外提供服务，显式设置为 `0.0.0.0`，并务必同时配置 `api_key`。
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// 每个 Provider 的最大并发请求数，0 表示不限制
    #[serde(default)]
    pub max_concurrent_requests: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 4000,
            host: default_host(),
            api_key: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_concurrent_requests: 0,
        }
    }
}

/// Secure-by-default bind address: loopback only.
fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFormat {
    Openai,
    Anthropic,
    Kiro,
}

fn default_format() -> ProviderFormat {
    ProviderFormat::Openai
}

fn default_max_reasoning_effort() -> String {
    "high".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider 唯一标识名称
    #[serde(default)]
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_format")]
    pub format: ProviderFormat,
    #[serde(default)]
    pub quirks: ProviderQuirks,
    /// 已废弃：模型路由已移至全局 Config.model_routes，此字段保留用于向后兼容反序列化
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_routes: Vec<ModelRoute>,
    /// Kiro (Amazon Q Developer) 专用配置，仅 format="kiro" 时需要
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiro_config: Option<KiroConfig>,
}

/// 旧格式 Provider（无 name 字段），用于向后兼容反序列化
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_format")]
    pub format: ProviderFormat,
    #[serde(default)]
    pub quirks: ProviderQuirks,
    #[serde(default)]
    pub model_routes: Vec<ModelRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiro_config: Option<KiroConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRoute {
    #[serde(rename = "match")]
    pub pattern: String,
    pub target: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderQuirks {
    /// 历史 assistant 消息是否都必须包含 reasoning_content（如 DeepSeek V4）
    #[serde(default)]
    pub reasoning_all_or_nothing: bool,
    /// 是否不支持 json_schema，需降级为 json_object（如 DeepSeek V4）
    #[serde(default)]
    pub no_json_schema: bool,
    /// 是否为非 OpenAI 内置推理模型也转发 reasoning_effort（如 DeepSeek V4）
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// Anthropic "max"/"adaptive" 推理强度映射到 OpenAI 的最大值
    /// 保守默认 "high"，若 provider 支持 "xhigh" 则可配置
    #[serde(default = "default_max_reasoning_effort")]
    pub max_reasoning_effort: String,
}

fn default_kiro_region() -> String {
    "us-east-1".to_string()
}

/// 多账户凭据池中的单个账户配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroAccountEntry {
    /// 唯一标识符（新增），用于持久化跟踪
    #[serde(default)]
    pub id: Option<String>,
    /// 认证方式: "social" | "idc" | "api_key"（默认继承 kiro_config.auth_method）
    #[serde(default)]
    pub auth_method: Option<String>,
    /// 刷新 token
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// OIDC 客户端 ID
    #[serde(default)]
    pub client_id: Option<String>,
    /// OIDC 客户端密钥
    #[serde(default)]
    pub client_secret: Option<String>,
    /// AWS profile ARN
    #[serde(default)]
    pub profile_arn: Option<String>,
    /// 区域（默认继承 kiro_config.region）
    #[serde(default)]
    pub region: Option<String>,
    /// API 区域（默认同 region）
    #[serde(default)]
    pub api_region: Option<String>,
    /// HTTP/SOCKS5 代理地址
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// 优先级（0 = 最高，默认 0）
    #[serde(default)]
    pub priority: Option<u32>,
    /// 是否禁用（默认 false）
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// Kiro (Amazon Q Developer) 专用配置
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KiroConfig {
    /// 认证方式: "social" | "idc" | "api_key"
    pub auth_method: String,
    /// 刷新 token（social 和 idc 时需要）
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// OIDC 客户端 ID（idc 时需要）
    #[serde(default)]
    pub client_id: Option<String>,
    /// OIDC 客户端密钥（idc 时需要）
    #[serde(default)]
    pub client_secret: Option<String>,
    /// AWS profile ARN（可选，自动从刷新响应获取）
    #[serde(default)]
    pub profile_arn: Option<String>,
    /// 区域（默认 us-east-1）
    #[serde(default = "default_kiro_region")]
    pub region: String,
    /// API 区域（默认同 region）
    #[serde(default)]
    pub api_region: Option<String>,
    /// 模型别名映射 (别名 → 真实模型 ID)
    #[serde(default)]
    pub model_aliases: Option<std::collections::HashMap<String, String>>,
    /// 隐藏模型列表（不暴露给客户端但可使用）
    #[serde(default)]
    pub hidden_models: Option<Vec<String>>,
    /// Kiro IDE 版本号（默认 "0.11.107"）
    #[serde(default)]
    pub kiro_version: Option<String>,
    /// HTTP/SOCKS5 代理地址
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Thinking 处理模式: "as_reasoning_content" | "remove" | "pass" | "strip_tags"
    #[serde(default)]
    pub thinking_mode: Option<String>,
    /// 是否启用 Web Search MCP 工具注入
    #[serde(default)]
    pub web_search_enabled: Option<bool>,
    /// 多账户凭据池配置（当 accounts 非空时启用多账户模式）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts: Option<Vec<KiroAccountEntry>>,
    /// 负载均衡模式: "priority"（按优先级）| "balanced"（轮询）| "smart"（智能评分），默认 "priority"
    #[serde(default)]
    pub load_balancing_mode: Option<String>,
    /// 是否注入 Agentic System Prompt（文件操作限制 + 时间戳），默认 false
    #[serde(default)]
    pub agentic_prompt_injection: Option<bool>,
    /// 是否过滤 Claude Code system prompt 中的环境噪声（gitStatus、Recent commits 等），默认 false
    #[serde(default)]
    pub filter_env_noise: Option<bool>,
    /// 是否移除 system prompt 中的边界标记（--- SYSTEM PROMPT --- 等），默认 false
    #[serde(default)]
    pub filter_strip_boundaries: Option<bool>,
    /// 首 token 超时（秒），默认 15
    #[serde(default)]
    pub first_token_timeout: Option<u64>,
    /// 流式读取超时（秒），默认 300
    #[serde(default)]
    pub streaming_read_timeout: Option<u64>,
    /// 首 token 最大重试次数，默认 3
    #[serde(default)]
    pub first_token_max_retries: Option<u32>,
    /// 429 配额冷却时间（秒），默认 300
    #[serde(default)]
    pub quota_cooldown_secs: Option<u64>,
    /// 健康分数每次失败衰减值，默认 20
    #[serde(default)]
    pub health_score_decay: Option<u32>,
    /// 健康分数每次成功恢复值，默认 10
    #[serde(default)]
    pub health_score_recovery: Option<u32>,
    /// 首选端点: "auto" | "kiro" | "codewhisperer" | "amazonq"，默认 "auto"
    #[serde(default)]
    pub preferred_endpoint: Option<String>,
    /// 429 时是否降级到其他端点，默认 true
    #[serde(default)]
    pub endpoint_fallback: Option<bool>,
    /// 是否保存调试请求 JSON（保存到 MODEL_PROXY_DEBUG_DIR/debug_requests/ 目录），默认 false
    #[serde(default)]
    pub debug_save_requests: Option<bool>,
    /// 是否启用 LLM Smart Summary（CONTENT_TOO_LONG 时用 Haiku 摘要再重试），默认 false
    #[serde(default)]
    pub smart_summary_enabled: Option<bool>,
    /// 是否启用主动配额检查（通过 getUsageLimits API），默认 false
    #[serde(default)]
    pub enable_quota_check: Option<bool>,
    /// 配额检查间隔（秒），默认 600（10 分钟）
    #[serde(default)]
    pub quota_check_interval_secs: Option<u64>,
}

impl ProviderConfig {
    /// Creates a placeholder ProviderConfig used as the serde default for the
    /// legacy `Config.provider` field. When a config uses the new `[[providers]]`
    /// format, this placeholder is simply left in place and ignored.
    pub fn placeholder() -> Self {
        Self {
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            format: ProviderFormat::Openai,
            quirks: ProviderQuirks::default(),
            model_routes: Vec::new(),
            kiro_config: None,
        }
    }

    /// 使用全局路由规则解析模型名称
    pub fn resolve_model_with_routes<'a>(
        &'a self,
        requested_model: Option<&str>,
        global_routes: &'a [ModelRoute],
    ) -> &'a str {
        matching_route(requested_model, global_routes)
            .map(|route| route.target.as_str())
            .unwrap_or(self.model.as_str())
    }

    /// 使用全局路由规则解析 reasoning_effort
    pub fn resolve_route_reasoning_effort_with_routes<'a>(
        &'a self,
        requested_model: Option<&str>,
        global_routes: &'a [ModelRoute],
    ) -> Option<&'a str> {
        matching_route(requested_model, global_routes)
            .and_then(|route| route.reasoning_effort.as_deref())
    }

    /// 向后兼容：使用 provider 自身的 model_routes（已废弃，优先使用全局路由）
    pub fn resolve_model<'a>(&'a self, requested_model: Option<&str>) -> &'a str {
        self.resolve_model_with_routes(requested_model, &self.model_routes)
    }

    /// 向后兼容：使用 provider 自身的 model_routes（已废弃，优先使用全局路由）
    pub fn resolve_route_reasoning_effort<'a>(
        &'a self,
        requested_model: Option<&str>,
    ) -> Option<&'a str> {
        self.resolve_route_reasoning_effort_with_routes(requested_model, &self.model_routes)
    }
}

/// 在路由列表中查找匹配的路由
fn matching_route<'a>(
    requested_model: Option<&str>,
    routes: &'a [ModelRoute],
) -> Option<&'a ModelRoute> {
    let requested_model = requested_model?;
    let requested_model = requested_model.to_ascii_lowercase();
    routes.iter().find(|route| {
        !route.pattern.is_empty() && requested_model.contains(&route.pattern.to_ascii_lowercase())
    })
}

impl Config {
    /// Normalize the config: migrate legacy `[provider]` to `providers` vec if needed.
    ///
    /// Rules:
    /// 1. If `providers` is non-empty, the new format wins and the legacy
    ///    `provider` field is ignored (the proxy reads providers via the
    ///    registry, never `config.provider`).
    /// 2. If `providers` is empty but the legacy `provider` field has data,
    ///    migrate it to providers with name="default" and active_provider="default".
    /// 3. If both are empty, that's fine - validate() will catch it.
    pub fn normalize(&mut self) {
        // Migrate the legacy `provider` field into `providers` only when no
        // new-format providers are present.
        if self.providers.is_empty() && !self.provider.base_url.is_empty() {
            let mut migrated = self.provider.clone();
            if migrated.name.is_empty() {
                migrated.name = "default".to_string();
            }
            self.providers = vec![migrated];
            self.active_provider = Some("default".to_string());
        }

        // Migrate provider-level model_routes to global model_routes if global is empty
        if self.model_routes.is_empty() {
            for provider in &mut self.providers {
                if !provider.model_routes.is_empty() {
                    self.model_routes.append(&mut provider.model_routes);
                }
            }
        }
        // Clear provider-level model_routes (they are now global)
        for provider in &mut self.providers {
            provider.model_routes.clear();
        }
    }

    /// Validate the config for correctness.
    ///
    /// Checks:
    /// - providers list is non-empty
    /// - providers list has ≤20 entries
    /// - Each provider name is non-empty and ≤64 chars
    /// - No duplicate names
    /// - Each provider has non-empty base_url, api_key, model
    /// - If active_provider is Some, it must reference an existing provider name
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Check providers non-empty
        if self.providers.is_empty() {
            return Err(ConfigError::NoProviders);
        }

        // Check max providers
        if self.providers.len() > MAX_PROVIDERS {
            return Err(ConfigError::TooManyProviders { max: MAX_PROVIDERS });
        }

        let mut seen_names = HashSet::new();

        for provider in &self.providers {
            // Validate name non-empty
            if provider.name.is_empty() {
                return Err(ConfigError::InvalidName {
                    reason: "name must not be empty".to_string(),
                });
            }

            // Validate name length
            if provider.name.len() > MAX_NAME_LENGTH {
                return Err(ConfigError::InvalidName {
                    reason: format!(
                        "'{}' exceeds max length of {} characters",
                        provider.name, MAX_NAME_LENGTH
                    ),
                });
            }

            // Check duplicate names
            if !seen_names.insert(&provider.name) {
                return Err(ConfigError::DuplicateName(provider.name.clone()));
            }

            // Validate required fields
            if provider.base_url.is_empty() {
                return Err(ConfigError::MissingField {
                    provider: provider.name.clone(),
                    field: "base_url".to_string(),
                });
            }
            if provider.api_key.is_empty() && provider.format != ProviderFormat::Kiro {
                return Err(ConfigError::MissingField {
                    provider: provider.name.clone(),
                    field: "api_key".to_string(),
                });
            }
            if provider.model.is_empty() {
                return Err(ConfigError::MissingField {
                    provider: provider.name.clone(),
                    field: "model".to_string(),
                });
            }
        }

        // Validate active_provider references a valid name
        if let Some(ref active) = self.active_provider {
            if !active.is_empty() && !self.providers.iter().any(|p| p.name == *active) {
                return Err(ConfigError::ProviderNotFound(active.clone()));
            }
        }

        Ok(())
    }

    /// Get the currently active ProviderConfig.
    ///
    /// - If active_provider is set and valid, return that provider
    /// - If active_provider is None/empty, return the first provider
    /// - If active_provider references invalid name, return ProviderNotFound error
    pub fn active_provider_config(&self) -> Result<&ProviderConfig, ConfigError> {
        match &self.active_provider {
            Some(name) if !name.is_empty() => self
                .providers
                .iter()
                .find(|p| p.name == *name)
                .ok_or_else(|| ConfigError::ProviderNotFound(name.clone())),
            _ => {
                // Return first provider, or error if empty
                self.providers.first().ok_or(ConfigError::NoProviders)
            }
        }
    }

    /// Serialize the config to TOML string, always using the new `[[providers]]` format.
    /// The legacy `[provider]` field is excluded from serialization.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        // Build a serializable struct that uses [[providers]] format
        #[derive(Serialize)]
        struct SerializableConfig<'a> {
            server: &'a ServerConfig,
            #[serde(skip_serializing_if = "Option::is_none")]
            active_provider: &'a Option<String>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            model_routes: &'a Vec<ModelRoute>,
            providers: &'a Vec<ProviderConfig>,
            logging: &'a LogConfig,
            #[serde(skip_serializing_if = "is_fallback_default")]
            fallback: &'a FallbackConfig,
        }

        fn is_fallback_default(f: &FallbackConfig) -> bool {
            !f.enabled
        }

        let serializable = SerializableConfig {
            server: &self.server,
            active_provider: &self.active_provider,
            model_routes: &self.model_routes,
            providers: &self.providers,
            logging: &self.logging,
            fallback: &self.fallback,
        };

        toml::to_string_pretty(&serializable).map_err(|e| ConfigError::Toml(e.to_string()))
    }

    pub fn load() -> anyhow::Result<Self> {
        // 首先尝试从当前可执行文件同级目录读取配置
        let exe_path = std::env::current_exe()?;
        let exe_dir = exe_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("无法获取可执行文件目录"))?;
        let config_path = exe_dir.join("config.toml");

        if config_path.exists() {
            info!("从 {:?} 加载配置文件", config_path);
            let content = std::fs::read_to_string(&config_path)?;
            let mut config: Config = toml::from_str(&content)?;
            config.normalize();
            config.validate().map_err(|e| anyhow::anyhow!("{}", e))?;
            return Ok(config);
        }

        // 回退到当前工作目录
        let cwd_config = Path::new("config.toml");
        if cwd_config.exists() {
            warn!("未在可执行文件目录找到配置，回退到当前工作目录");
            let content = std::fs::read_to_string(cwd_config)?;
            let mut config: Config = toml::from_str(&content)?;
            config.normalize();
            config.validate().map_err(|e| anyhow::anyhow!("{}", e))?;
            return Ok(config);
        }

        anyhow::bail!(
            "未找到配置文件 config.toml，请在可执行文件同级目录或当前工作目录放置配置文件"
        )
    }

    pub fn load_from_path(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&content)?;
        config.normalize();
        config.validate().map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(config)
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;
    use crate::logging::LogConfig;
    use proptest::prelude::*;

    fn make_provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            base_url: "https://example.com".to_string(),
            api_key: "sk-test".to_string(),
            model: "test-model".to_string(),
            format: ProviderFormat::Openai,
            quirks: ProviderQuirks::default(),
            model_routes: Vec::new(),
            kiro_config: None,
        }
    }

    fn make_config(providers: Vec<ProviderConfig>) -> Config {
        Config {
            server: ServerConfig::default(),
            provider: ProviderConfig::placeholder(),
            active_provider: providers.first().map(|p| p.name.clone()),
            providers,
            model_routes: Vec::new(),
            model_routes_enabled: true,
            logging: LogConfig::default(),
            fallback: FallbackConfig::default(),
        }
    }

    // --- Basic unit tests ---

    #[test]
    fn server_config_default_host_is_loopback() {
        // Secure default: bind to localhost only.
        assert_eq!(ServerConfig::default().host, "127.0.0.1");
    }

    #[test]
    fn server_config_host_defaults_when_absent_in_toml() {
        // Older config files have no `host` key; deserialization must fill the
        // secure default rather than failing or binding to all interfaces.
        let toml_str = "port = 4000\n";
        let server: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(server.host, "127.0.0.1");
        assert_eq!(server.port, 4000);
    }

    #[test]
    fn server_config_host_can_be_overridden() {
        let toml_str = "port = 4000\nhost = \"0.0.0.0\"\n";
        let server: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(server.host, "0.0.0.0");
    }

    #[test]
    fn valid_config_passes_validation() {
        let mut config = make_config(vec![make_provider("test")]);
        config.normalize();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn empty_providers_fails_validation() {
        let config = make_config(vec![]);
        assert!(matches!(config.validate(), Err(ConfigError::NoProviders)));
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let config = make_config(vec![make_provider("dup"), make_provider("dup")]);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicateName(_))
        ));
    }

    #[test]
    fn active_provider_config_returns_correct_provider() {
        let mut config = make_config(vec![make_provider("a"), make_provider("b")]);
        config.active_provider = Some("b".to_string());
        config.normalize();
        let active = config.active_provider_config().unwrap();
        assert_eq!(active.name, "b");
    }

    #[test]
    fn active_provider_unset_returns_first() {
        let mut config = make_config(vec![make_provider("first"), make_provider("second")]);
        config.active_provider = None;
        config.normalize();
        let active = config.active_provider_config().unwrap();
        assert_eq!(active.name, "first");
    }

    #[test]
    fn active_provider_invalid_name_returns_error() {
        let mut config = make_config(vec![make_provider("a")]);
        config.active_provider = Some("nonexistent".to_string());
        config.normalize();
        assert!(matches!(
            config.active_provider_config(),
            Err(ConfigError::ProviderNotFound(_))
        ));
    }

    #[test]
    fn legacy_provider_migrates() {
        let mut config = Config {
            server: ServerConfig::default(),
            provider: ProviderConfig {
                name: String::new(),
                base_url: "https://api.example.com".to_string(),
                api_key: "sk-key".to_string(),
                model: "model-1".to_string(),
                format: ProviderFormat::Openai,
                quirks: ProviderQuirks::default(),
                model_routes: Vec::new(),
                kiro_config: None,
            },
            active_provider: None,
            providers: Vec::new(),
            model_routes: Vec::new(),
            model_routes_enabled: true,
            logging: LogConfig::default(),
            fallback: FallbackConfig::default(),
        };
        config.normalize();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].name, "default");
        assert_eq!(config.active_provider, Some("default".to_string()));
    }

    #[test]
    fn serialization_produces_new_format() {
        let mut config = make_config(vec![make_provider("test")]);
        config.normalize();
        let toml_str = config.to_toml_string().unwrap();
        assert!(toml_str.contains("[[providers]]"));
        assert!(!toml_str.contains("[provider]"));
    }

    #[test]
    fn fallback_config_defaults() {
        let config = make_config(vec![make_provider("test")]);
        assert!(!config.fallback.enabled);
        assert_eq!(config.fallback.on_status_codes, vec![429, 500, 502, 503]);
        assert_eq!(config.fallback.max_attempts, 3);
    }

    #[test]
    fn max_concurrent_requests_defaults_to_zero() {
        let config = make_config(vec![make_provider("test")]);
        assert_eq!(config.server.max_concurrent_requests, 0);
    }

    #[test]
    fn too_many_providers_rejected() {
        let providers: Vec<ProviderConfig> =
            (0..21).map(|i| make_provider(&format!("p{}", i))).collect();
        let config = make_config(providers);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::TooManyProviders { .. })
        ));
    }

    #[test]
    fn empty_name_is_rejected() {
        let mut p = make_provider("");
        p.name = String::new();
        let config = make_config(vec![p]);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn long_name_is_rejected() {
        let long_name = "a".repeat(65);
        let config = make_config(vec![make_provider(&long_name)]);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn missing_base_url_produces_error() {
        let mut p = make_provider("test");
        p.base_url = String::new();
        let config = make_config(vec![p]);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::MissingField { field, .. }) if field == "base_url"
        ));
    }

    #[test]
    fn missing_api_key_produces_error() {
        let mut p = make_provider("test");
        p.api_key = String::new();
        let config = make_config(vec![p]);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::MissingField { field, .. }) if field == "api_key"
        ));
    }

    #[test]
    fn kiro_provider_allows_empty_api_key() {
        let mut p = make_provider("kiro");
        p.format = ProviderFormat::Kiro;
        p.api_key = String::new();
        p.kiro_config = Some(KiroConfig {
            auth_method: "social".to_string(),
            refresh_token: Some("refresh".to_string()),
            client_id: None,
            client_secret: None,
            profile_arn: None,
            region: "us-east-1".to_string(),
            api_region: None,
            model_aliases: None,
            hidden_models: None,
            kiro_version: None,
            proxy_url: None,
            thinking_mode: None,
            web_search_enabled: None,
            accounts: None,
            load_balancing_mode: None,
            agentic_prompt_injection: None,
            filter_env_noise: None,
            filter_strip_boundaries: None,
            first_token_timeout: None,
            streaming_read_timeout: None,
            first_token_max_retries: None,
            quota_cooldown_secs: None,
            health_score_decay: None,
            health_score_recovery: None,
            preferred_endpoint: None,
            endpoint_fallback: None,
            debug_save_requests: None,
            smart_summary_enabled: None,
            enable_quota_check: None,
            quota_check_interval_secs: None,
        });
        let config = make_config(vec![p]);

        assert!(config.validate().is_ok());
    }

    #[test]
    fn missing_model_produces_error() {
        let mut p = make_provider("test");
        p.model = String::new();
        let config = make_config(vec![p]);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::MissingField { field, .. }) if field == "model"
        ));
    }

    #[test]
    fn model_route_matching_is_case_insensitive() {
        let routes = vec![ModelRoute {
            pattern: "Sonnet".to_string(),
            target: "target-model".to_string(),
            reasoning_effort: None,
        }];
        let provider = make_provider("test");
        let result = provider.resolve_model_with_routes(Some("claude-SONNET-4"), &routes);
        assert_eq!(result, "target-model");
    }

    #[test]
    fn model_route_no_match_returns_default() {
        let routes = vec![ModelRoute {
            pattern: "sonnet".to_string(),
            target: "target-model".to_string(),
            reasoning_effort: None,
        }];
        let provider = make_provider("test");
        let result = provider.resolve_model_with_routes(Some("gpt-4o"), &routes);
        assert_eq!(result, "test-model"); // Falls back to provider default
    }

    // --- Property-based tests ---

    /// Generate a valid provider name (1-64 alphanumeric + underscore/hyphen)
    fn arb_provider_name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_-]{0,15}"
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..Default::default() })]

        /// Config with unique provider names always passes validation.
        #[test]
        fn valid_unique_names_pass(
            names in proptest::collection::hash_set(arb_provider_name(), 1..=5)
        ) {
            let providers: Vec<ProviderConfig> = names.iter().map(|n| make_provider(n)).collect();
            let mut config = make_config(providers);
            config.normalize();
            prop_assert!(config.validate().is_ok());
        }

        /// Config serialization round-trip preserves all data.
        #[test]
        fn config_serialization_round_trip(
            names in proptest::collection::hash_set(arb_provider_name(), 1..=3),
            port in 1024u16..=65535u16,
        ) {
            let providers: Vec<ProviderConfig> = names.iter().map(|n| make_provider(n)).collect();
            let active = providers[0].name.clone();
            let mut config = Config {
                server: ServerConfig { port, ..Default::default() },
                provider: ProviderConfig::placeholder(),
                active_provider: Some(active.clone()),
                providers,
                model_routes: Vec::new(),
                model_routes_enabled: true,
                logging: LogConfig::default(),
                fallback: FallbackConfig::default(),
            };
            config.normalize();

            let toml_str = config.to_toml_string().unwrap();
            let mut parsed: Config = toml::from_str(&toml_str).unwrap();
            parsed.normalize();

            prop_assert_eq!(parsed.server.port, port);
            prop_assert_eq!(parsed.active_provider.as_deref(), Some(active.as_str()));
            prop_assert_eq!(parsed.providers.len(), config.providers.len());
            for (orig, parsed) in config.providers.iter().zip(parsed.providers.iter()) {
                prop_assert_eq!(&orig.name, &parsed.name);
                prop_assert_eq!(&orig.base_url, &parsed.base_url);
                prop_assert_eq!(&orig.model, &parsed.model);
            }
        }

        /// active_provider_config always returns a provider from the providers list.
        #[test]
        fn active_provider_always_in_list(
            names in proptest::collection::hash_set(arb_provider_name(), 2..=5),
            active_idx in 0usize..5usize,
        ) {
            let providers: Vec<ProviderConfig> = names.iter().map(|n| make_provider(n)).collect();
            let idx = active_idx % providers.len();
            let active_name = providers[idx].name.clone();
            let mut config = make_config(providers.clone());
            config.active_provider = Some(active_name.clone());
            config.normalize();

            let result = config.active_provider_config().unwrap();
            prop_assert_eq!(&result.name, &active_name);
            prop_assert!(providers.iter().any(|p| p.name == result.name));
        }
    }
}
