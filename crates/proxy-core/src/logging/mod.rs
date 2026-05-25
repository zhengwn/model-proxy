pub mod collector;
pub mod config;
pub mod entry;
pub mod file_logger;
pub mod truncate;

pub use collector::LogCollector;
pub use config::{LogConfig, LogLevel};
pub use entry::LogEntry;
pub use file_logger::FileLogger;
pub use truncate::truncate_body;

#[cfg(test)]
mod tests;
