//! tracing setup: stdout + daily rolling file in `<data_dir>/logs/`
//! (`antbot.log.YYYY-MM-DD`, the last `max_files` days kept). Filter from
//! `RUST_LOG` (default `info`). JSON output optional (both outputs). ANSI
//! colors on stdout only when it is a terminal.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::Context;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub const LOG_FILE_PREFIX: &str = "antbot.log";
pub const DEFAULT_FILTER: &str = "info";

/// Daily rolling file appender in `<data_dir>/logs` (`max_files` = 0 keeps
/// every file).
pub fn file_appender(data_dir: &Path, max_files: usize) -> anyhow::Result<RollingFileAppender> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("creating log dir {}", log_dir.display()))?;
    let mut b = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX);
    if max_files > 0 {
        b = b.max_log_files(max_files);
    }
    b.build(&log_dir).context("creating the rolling log file")
}

/// `RUST_LOG`, or [`DEFAULT_FILTER`] when unset / invalid.
pub fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Initialise global logging. Keep the returned guard alive for the whole
/// process so the file writer flushes on exit.
pub fn init(data_dir: &Path, json: bool, max_files: usize) -> anyhow::Result<WorkerGuard> {
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender(data_dir, max_files)?);
    // Colors only on an interactive terminal: `docker logs`, journald and
    // redirected output get plain text.
    let ansi = std::io::stdout().is_terminal();

    let (stdout_layer, file_layer) = if json {
        (
            fmt::layer()
                .json()
                .with_ansi(false)
                .with_filter(env_filter())
                .boxed(),
            fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(env_filter())
                .boxed(),
        )
    } else {
        (
            fmt::layer()
                .with_ansi(ansi)
                .with_filter(env_filter())
                .boxed(),
            fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(env_filter())
                .boxed(),
        )
    };

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .try_init()?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn file_appender_writes_dated_file_in_logs_dir() {
        let d = tempfile::tempdir().unwrap();
        let mut a = file_appender(d.path(), 7).unwrap();
        writeln!(a, "hello").unwrap();
        a.flush().unwrap();
        let names: Vec<String> = std::fs::read_dir(d.path().join("logs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1);
        let n = &names[0];
        assert!(n.starts_with("antbot.log."), "{n}");
        // antbot.log.YYYY-MM-DD
        assert_eq!(n.len(), "antbot.log.".len() + 10, "{n}");
    }

    #[test]
    fn json_layer_emits_structured_fields() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let b2 = buf.clone();
        let sub = tracing_subscriber::registry().with(
            fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(move || b2.clone()),
        );
        tracing::subscriber::with_default(sub, || {
            tracing::info!(premium_bps = -72.5, phase = "flat", "heartbeat");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["fields"]["message"], "heartbeat");
        assert_eq!(v["fields"]["premium_bps"], -72.5);
        assert_eq!(v["level"], "INFO");
    }
}
