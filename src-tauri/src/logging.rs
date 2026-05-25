use proxy_core::logging::LogEntry;
use tauri::Emitter;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Background task that forwards log entries as Tauri events to the frontend.
///
/// Receives `LogEntry` values from the broadcast channel and emits them as
/// `"proxy-log"` Tauri events. Handles lagged receivers by skipping missed
/// entries and shuts down gracefully when the cancellation token is triggered
/// or the channel is closed.
pub async fn event_emitter_task(
    mut receiver: broadcast::Receiver<LogEntry>,
    app_handle: tauri::AppHandle,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = receiver.recv() => {
                match result {
                    Ok(entry) => {
                        let _ = app_handle.emit("proxy-log", &entry);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
