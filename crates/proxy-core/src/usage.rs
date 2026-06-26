//! Persistent per-day token-usage counter for the GUI usage heatmap.
//!
//! A background [`UsageTracker`] subscribes to the same log broadcast channel as
//! the file logger and accumulates per-day **token usage** (`LogEntry.token_count`).
//! Totals are persisted to `usage_daily.json` in the log directory,
//! **independently of log retention**, so the heatmap can show months of history
//! even though raw log files are purged after `retention_days`.
//!
//! Notes:
//! - Token counts are currently captured for streaming responses (the dominant
//!   traffic for IDE clients). Non-stream responses and errors carry no token
//!   count and contribute nothing.
//! - Entries are only emitted on the broadcast when logging is enabled and the
//!   status passes the level filter (`LogCollector::should_log`). Disabling
//!   logging or switching to errors-only stops accumulation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::Utc;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::logging::{LogConfig, LogEntry};

/// File name (inside the log directory) holding the persisted daily counts.
const USAGE_FILE: &str = "usage_daily.json";

/// How often, at most, the in-memory counts are written to disk while running.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// One calendar day's token usage, returned to the GUI for the heatmap.
#[derive(Debug, Clone, Serialize)]
pub struct DailyUsage {
    /// Calendar day in `YYYY-MM-DD` (UTC).
    pub date: String,
    /// Total tokens used on that day.
    pub count: u64,
}

/// Resolve the directory used to persist usage stats — identical to the log dir
/// so everything lives together. Falls back to `{app_data_dir}/logs`.
fn resolve_log_dir(config: &Arc<ArcSwap<LogConfig>>, app_data_dir: &Path) -> PathBuf {
    let cfg = config.load();
    match &cfg.log_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => app_data_dir.join("logs"),
    }
}

/// Read the persisted daily usage map from disk. Returns an empty map when the
/// file is missing or cannot be parsed.
fn read_usage(log_dir: &Path) -> BTreeMap<String, u64> {
    let path = log_dir.join(USAGE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    }
}

/// Load the daily usage as a sorted vector. Used by the Tauri query command; safe
/// to call whether or not the proxy service is currently running.
pub fn load_daily_usage(
    config: &Arc<ArcSwap<LogConfig>>,
    app_data_dir: &Path,
) -> Vec<DailyUsage> {
    let log_dir = resolve_log_dir(config, app_data_dir);
    read_usage(&log_dir)
        .into_iter()
        .map(|(date, count)| DailyUsage { date, count })
        .collect()
}

/// Background task that accumulates and persists the per-day request count.
pub struct UsageTracker;

impl UsageTracker {
    /// Run until `cancel` is triggered or the broadcast channel closes.
    pub async fn run(
        mut receiver: broadcast::Receiver<LogEntry>,
        config: Arc<ArcSwap<LogConfig>>,
        app_data_dir: PathBuf,
        cancel: CancellationToken,
    ) {
        let log_dir = resolve_log_dir(&config, &app_data_dir);
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            tracing::error!("Failed to create usage directory {:?}: {}", log_dir, e);
        }

        let mut counts = read_usage(&log_dir);
        let mut dirty = false;
        let mut last_flush = Instant::now();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    break;
                }
                result = receiver.recv() => {
                    match result {
                        Ok(entry) => {
                            // Accumulate token usage per day. Entries without token
                            // info (errors, non-stream responses) contribute nothing.
                            let tokens = entry.token_count.unwrap_or(0);
                            if tokens > 0 {
                                let today = Utc::now().date_naive().format("%Y-%m-%d").to_string();
                                *counts.entry(today).or_insert(0) += tokens;
                                dirty = true;
                                if last_flush.elapsed() >= FLUSH_INTERVAL {
                                    Self::flush(&config, &app_data_dir, &counts);
                                    dirty = false;
                                    last_flush = Instant::now();
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Heatmap tolerates a small undercount on overflow.
                            tracing::warn!("UsageTracker lagged, skipped {} entries", n);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }

        // Persist any counts accumulated since the last flush before exiting.
        if dirty {
            Self::flush(&config, &app_data_dir, &counts);
        }
    }

    /// Write the current counts to `usage_daily.json` (whole-file replace).
    fn flush(
        config: &Arc<ArcSwap<LogConfig>>,
        app_data_dir: &Path,
        counts: &BTreeMap<String, u64>,
    ) {
        let log_dir = resolve_log_dir(config, app_data_dir);
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            tracing::error!("Failed to create usage directory {:?}: {}", log_dir, e);
            return;
        }
        let path = log_dir.join(USAGE_FILE);
        match serde_json::to_string(counts) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::error!("Failed to write usage file {:?}: {}", path, e);
                }
            }
            Err(e) => tracing::error!("Failed to serialize usage counts: {}", e),
        }
    }
}
