//! Logging: stdout, plus a daily-rotated file (default `<vault>/logs/`).
//!
//! The journal is the authoritative, structured record of what the guard
//! did; this log is the human-readable trail around it (startup, policy,
//! scans, warnings), and it outlives the terminal the daemon started in.
//!
//! The file writer runs on its own thread and is lossless: if it falls
//! behind, logging calls wait rather than drop lines.

use anyhow::{Context, Result};
use std::path::Path;
use tracing_appender::{
    non_blocking::{NonBlockingBuilder, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Install the global subscriber. With `file = Some((dir, keep))`, also log
/// to `dir/ultnasd.<date>.log`, keeping the newest `keep` files.
///
/// Hold the returned guard until exit: dropping it flushes the file.
pub fn init(level: &str, file: Option<(&Path, usize)>) -> Result<Option<WorkerGuard>> {
    let stdout = fmt::layer().with_filter(EnvFilter::new(level));

    let (file_layer, guard) = match file {
        Some((dir, keep)) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating log directory {}", dir.display()))?;
            let appender = RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix("ultnasd")
                .filename_suffix("log")
                .max_log_files(keep.max(1))
                .build(dir)
                .with_context(|| format!("opening log file in {}", dir.display()))?;
            let (writer, guard) = NonBlockingBuilder::default().lossy(false).finish(appender);
            let layer = fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(EnvFilter::new(level));
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(stdout)
        .with(file_layer)
        .init();
    Ok(guard)
}
