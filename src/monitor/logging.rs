//! tracing setup: stdout + daily rolling file in `<data_dir>/logs/`.
//! Filter from `RUST_LOG` (default `info`). JSON output optional.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub const LOG_FILE_PREFIX: &str = "antbot.log";

/// Initialise global logging. Keep the returned guard alive for the whole
/// process so the file writer flushes on exit.
pub fn init(data_dir: &Path, json: bool) -> anyhow::Result<WorkerGuard> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, LOG_FILE_PREFIX);
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let (stdout_layer, file_layer) = if json {
        (
            fmt::layer().json().with_filter(filter()).boxed(),
            fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(filter())
                .boxed(),
        )
    } else {
        (
            fmt::layer().with_filter(filter()).boxed(),
            fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(filter())
                .boxed(),
        )
    };

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .try_init()?;
    Ok(guard)
}
