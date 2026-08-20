//! Tracing setup that writes structured internal logs to a per-launch file:
//! `<MA_LOG_FILE_DIR>/<yyyyMMdd-HHmmss>.log`. Internal events go only to the
//! file (never stdout) — stdout is reserved for the model's text + tool marks
//! (see [`crate::out`]).

use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Timestamp used as the log file name, e.g. `20260820-121530`.
fn ts() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Initialise the file logger. Returns `None` (with no guard) when no
/// `MA_LOG_FILE_DIR` is configured, in which case internal logs are dropped —
/// stdout remains the only output path.
pub fn init(log_dir: Option<&PathBuf>, level: &str) -> Option<WorkerGuard> {
    let dir = log_dir?;
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("{}.log", ts()));

    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            // Not fatal: fall back to no file logging but keep stdout working.
            tracing::warn!("failed to create log file {}: {e}", path.display());
            return None;
        }
    };

    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::new(format!("ma={level},rmcp=warn,h2=warn,hyper=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .init();

    tracing::info!("log file: {}", path.display());
    Some(guard)
}
