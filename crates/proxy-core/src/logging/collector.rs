use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;

use super::config::{LogConfig, LogLevel};
use super::entry::LogEntry;

pub struct LogCollector {
    pub sender: broadcast::Sender<LogEntry>,
    pub config: Arc<ArcSwap<LogConfig>>,
}

impl LogCollector {
    pub fn new(config: Arc<ArcSwap<LogConfig>>, capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender, config }
    }

    /// Attempt to send a log entry. Returns silently if no receivers or channel full.
    pub fn emit(&self, entry: LogEntry) {
        let _ = self.sender.send(entry);
    }

    /// Check if logging should occur for the given status code.
    /// Returns `false` when logging is disabled or when the level filter excludes the status.
    pub fn should_log(&self, status: u16) -> bool {
        let cfg = self.config.load();
        if !cfg.enabled {
            return false;
        }
        match cfg.level {
            LogLevel::All => true,
            LogLevel::ErrorsOnly => status >= 400,
        }
    }
}
