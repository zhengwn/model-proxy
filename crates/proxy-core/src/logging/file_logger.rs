use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{NaiveDate, Utc};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::config::LogConfig;
use super::entry::LogEntry;

/// Cached file handle with its associated date and path for daily rotation.
struct CachedFile {
    file: File,
    date: NaiveDate,
    log_dir: PathBuf,
}

/// Background task that writes log entries to daily-rotated JSONL files.
pub struct FileLogger {
    config: Arc<ArcSwap<LogConfig>>,
    app_data_dir: PathBuf,
    /// Cached file handle to avoid re-opening on every write.
    cached: Option<CachedFile>,
}

impl FileLogger {
    /// Main entry point: runs the file logger as an async task.
    ///
    /// On startup:
    /// - Resolves the log directory from config (or falls back to `{app_data_dir}/logs`)
    /// - Creates the directory recursively
    /// - Purges files older than `retention_days`
    ///
    /// Then loops receiving entries from the broadcast channel, appending each as
    /// a JSON line to the current day's file.
    pub async fn run(
        mut receiver: broadcast::Receiver<LogEntry>,
        config: Arc<ArcSwap<LogConfig>>,
        app_data_dir: PathBuf,
        cancel: CancellationToken,
    ) {
        // Resolve initial log directory from config and create it
        let initial_log_dir = Self::resolve_log_dir(&config, &app_data_dir);
        if let Err(e) = fs::create_dir_all(&initial_log_dir).await {
            tracing::error!(
                "Failed to create log directory {:?}: {}",
                initial_log_dir,
                e
            );
            return;
        }

        let mut logger = FileLogger {
            config,
            app_data_dir,
            cached: None,
        };

        // Purge old files on startup
        logger.purge_old_files().await;

        // Main loop
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    break;
                }
                result = receiver.recv() => {
                    match result {
                        Ok(entry) => {
                            logger.write_entry(&entry).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("FileLogger lagged, skipped {} entries", n);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Get or open the file handle for the current day.
    /// Reuses the cached handle if the date and log_dir haven't changed.
    async fn get_or_open_file(&mut self, today: NaiveDate, log_dir: &PathBuf) -> Option<&mut File> {
        // Check if cached handle is still valid (same date and same directory)
        let needs_reopen = match &self.cached {
            Some(cached) => cached.date != today || cached.log_dir != *log_dir,
            None => true,
        };

        if needs_reopen {
            // Ensure the directory exists (may have changed via hot-reload)
            if let Err(e) = fs::create_dir_all(log_dir).await {
                tracing::error!("Failed to create log directory {:?}: {}", log_dir, e);
                return None;
            }

            let filename = generate_log_filename(today);
            let filepath = log_dir.join(&filename);

            let file = match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&filepath)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open log file {:?}: {}", filepath, e);
                    self.cached = None;
                    return None;
                }
            };

            self.cached = Some(CachedFile {
                file,
                date: today,
                log_dir: log_dir.clone(),
            });
        }

        self.cached.as_mut().map(|c| &mut c.file)
    }

    /// Write a single log entry as a JSON line to the current day's file.
    /// Re-reads log_dir from config on each call to support hot-reload.
    async fn write_entry(&mut self, entry: &LogEntry) {
        let log_dir = Self::resolve_log_dir(&self.config, &self.app_data_dir);
        let today = Utc::now().date_naive();

        let json_line = match serde_json::to_string(entry) {
            Ok(json) => json,
            Err(e) => {
                tracing::error!("Failed to serialize log entry: {}", e);
                return;
            }
        };

        let file = match self.get_or_open_file(today, &log_dir).await {
            Some(f) => f,
            None => return,
        };

        let mut line = json_line;
        line.push('\n');
        if let Err(e) = file.write_all(line.as_bytes()).await {
            tracing::error!("Failed to write log entry: {}", e);
            // Invalidate cache so next write attempts to reopen
            self.cached = None;
        }
    }

    /// Delete log files older than `retention_days` from the log directory.
    /// Re-reads retention_days and log_dir from config to support hot-reload.
    async fn purge_old_files(&self) {
        let cfg = self.config.load();
        let retention_days = cfg.retention_days;
        let log_dir = Self::resolve_log_dir(&self.config, &self.app_data_dir);
        let reference_date = Utc::now().date_naive();

        let mut entries = match fs::read_dir(&log_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!("Failed to read log directory {:?}: {}", log_dir, e);
                return;
            }
        };

        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let file_name = entry.file_name();
                    let name = file_name.to_string_lossy();
                    if should_purge(&name, retention_days, reference_date) {
                        let path = entry.path();
                        if let Err(e) = fs::remove_file(&path).await {
                            tracing::error!("Failed to delete old log file {:?}: {}", path, e);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("Error reading log directory entry: {}", e);
                    break;
                }
            }
        }
    }

    /// Resolve the log directory from the current config snapshot.
    /// Falls back to `{app_data_dir}/logs` when log_dir is not configured.
    fn resolve_log_dir(config: &Arc<ArcSwap<LogConfig>>, app_data_dir: &Path) -> PathBuf {
        let cfg = config.load();
        match &cfg.log_dir {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => app_data_dir.join("logs"),
        }
    }
}

/// Generate the log filename for a given date.
///
/// Returns a string in the format `proxy-YYYY-MM-DD.jsonl`.
pub(crate) fn generate_log_filename(date: NaiveDate) -> String {
    format!("proxy-{}.jsonl", date.format("%Y-%m-%d"))
}

/// Determine whether a log file should be purged based on its filename,
/// the configured retention period, and a reference date.
///
/// Returns `true` if the file matches the `proxy-YYYY-MM-DD.jsonl` pattern
/// and its embedded date is strictly more than `retention_days` days before
/// the reference date.
pub(crate) fn should_purge(filename: &str, retention_days: u32, reference_date: NaiveDate) -> bool {
    // Must match pattern: proxy-YYYY-MM-DD.jsonl
    let stem = match filename
        .strip_prefix("proxy-")
        .and_then(|s| s.strip_suffix(".jsonl"))
    {
        Some(s) => s,
        None => return false,
    };

    let file_date = match NaiveDate::parse_from_str(stem, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return false,
    };

    let age_days = (reference_date - file_date).num_days();
    age_days > retention_days as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn test_generate_log_filename() {
        let date = NaiveDate::from_ymd_opt(2024, 7, 1).unwrap();
        assert_eq!(generate_log_filename(date), "proxy-2024-07-01.jsonl");
    }

    #[test]
    fn test_generate_log_filename_single_digit_month_day() {
        let date = NaiveDate::from_ymd_opt(2024, 1, 5).unwrap();
        assert_eq!(generate_log_filename(date), "proxy-2024-01-05.jsonl");
    }

    #[test]
    fn test_should_purge_old_file() {
        let reference = NaiveDate::from_ymd_opt(2024, 7, 10).unwrap();
        // File from 2024-07-01 is 9 days old, retention is 7
        assert!(should_purge("proxy-2024-07-01.jsonl", 7, reference));
    }

    #[test]
    fn test_should_not_purge_recent_file() {
        let reference = NaiveDate::from_ymd_opt(2024, 7, 10).unwrap();
        // File from 2024-07-05 is 5 days old, retention is 7
        assert!(!should_purge("proxy-2024-07-05.jsonl", 7, reference));
    }

    #[test]
    fn test_should_not_purge_exactly_at_boundary() {
        let reference = NaiveDate::from_ymd_opt(2024, 7, 10).unwrap();
        // File from 2024-07-03 is exactly 7 days old, retention is 7
        // "strictly more than retention_days" means this should NOT be purged
        assert!(!should_purge("proxy-2024-07-03.jsonl", 7, reference));
    }

    #[test]
    fn test_should_not_purge_non_matching_filename() {
        let reference = NaiveDate::from_ymd_opt(2024, 7, 10).unwrap();
        assert!(!should_purge("other-file.txt", 7, reference));
        assert!(!should_purge("proxy-invalid.jsonl", 7, reference));
        assert!(!should_purge("proxy-2024-13-01.jsonl", 7, reference));
    }

    #[test]
    fn test_should_not_purge_todays_file() {
        let reference = NaiveDate::from_ymd_opt(2024, 7, 10).unwrap();
        assert!(!should_purge("proxy-2024-07-10.jsonl", 7, reference));
    }
}
