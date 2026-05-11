use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{info, warn};

pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub provider: ProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub port: u16,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
}

fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFormat {
    Openai,
    Anthropic,
}

fn default_format() -> ProviderFormat {
    ProviderFormat::Openai
}

fn default_max_reasoning_effort() -> String {
    "high".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_format")]
    pub format: ProviderFormat,
    #[serde(default)]
    pub quirks: ProviderQuirks,
    #[serde(default)]
    pub model_routes: Vec<ModelRoute>,
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

impl ProviderConfig {
    fn matching_route<'a>(&'a self, requested_model: Option<&str>) -> Option<&'a ModelRoute> {
        let requested_model = requested_model?;
        let requested_model = requested_model.to_ascii_lowercase();
        self.model_routes.iter().find(|route| {
            !route.pattern.is_empty()
                && requested_model.contains(&route.pattern.to_ascii_lowercase())
        })
    }

    pub fn resolve_model<'a>(&'a self, requested_model: Option<&str>) -> &'a str {
        self.matching_route(requested_model)
            .map(|route| route.target.as_str())
            .unwrap_or(self.model.as_str())
    }

    pub fn resolve_route_reasoning_effort<'a>(
        &'a self,
        requested_model: Option<&str>,
    ) -> Option<&'a str> {
        self.matching_route(requested_model)
            .and_then(|route| route.reasoning_effort.as_deref())
    }
}

impl Config {
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
            let config: Config = toml::from_str(&content)?;
            return Ok(config);
        }

        // 回退到当前工作目录
        let cwd_config = Path::new("config.toml");
        if cwd_config.exists() {
            warn!("未在可执行文件目录找到配置，回退到当前工作目录");
            let content = std::fs::read_to_string(cwd_config)?;
            let config: Config = toml::from_str(&content)?;
            return Ok(config);
        }

        anyhow::bail!(
            "未找到配置文件 config.toml，请在可执行文件同级目录或当前工作目录放置配置文件"
        )
    }
}
