use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_level")]
    pub level: LogLevel,
    #[serde(default)]
    pub log_dir: Option<String>,
    #[serde(default = "default_record_body")]
    pub record_body: bool,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    All,
    ErrorsOnly,
}

fn default_enabled() -> bool {
    true
}

fn default_level() -> LogLevel {
    LogLevel::All
}

fn default_record_body() -> bool {
    false
}

fn default_max_body_bytes() -> usize {
    4096
}

fn default_retention_days() -> u32 {
    7
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            level: default_level(),
            log_dir: None,
            record_body: default_record_body(),
            max_body_bytes: default_max_body_bytes(),
            retention_days: default_retention_days(),
        }
    }
}
