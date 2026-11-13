//! Monitoring: tracing setup, CSV journals, Telegram alerts, daily summary.
//!
//! * [`logging`]: stdout + daily rolling file in `<data_dir>/logs/`,
//!   `RUST_LOG` filter, optional JSON;
//! * [`csv`]: `signals.csv`, `orders.csv`, `trades.csv` in the data dir
//!   (header once, appended across restarts);
//! * [`telegram`]: non-blocking, rate-limited alert queue (disabled with a
//!   log line when `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` are unset);
//! * [`alerts`]: the alert texts (pure functions, unit tested) and the
//!   daily summary.

pub mod alerts;
pub mod csv;
pub mod logging;
pub mod telegram;

use crate::strategy::signal::{Command, OrderUpdate, TradeRecord};

/// What the main loop talks to: CSV journals + alert queue.
#[derive(Debug, Default)]
pub struct Monitor {
    pub journal: csv::Journal,
    pub alerter: telegram::Alerter,
    pub coin: String,
}

impl Monitor {
    pub fn new(journal: csv::Journal, alerter: telegram::Alerter, coin: &str) -> Self {
        Self {
            journal,
            alerter,
            coin: coin.to_string(),
        }
    }

    /// No files, no alerts (tests, tools).
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Operator warning: log (WARN, `alert = true`) + Telegram.
    pub fn alert(&self, msg: &str) {
        tracing::warn!(alert = true, "{msg}");
        self.alerter.send(alerts::warning(msg));
    }

    /// Error: log (ERROR) + Telegram.
    pub fn error(&self, msg: &str) {
        tracing::error!(alert = true, "{msg}");
        self.alerter.send(alerts::error(msg));
    }

    /// Telegram only (the caller already logged the event).
    pub fn notify(&self, text: String) {
        self.alerter.send(text);
    }

    pub fn command(&mut self, ts_ms: u64, c: &Command) {
        self.journal.command(ts_ms, c);
    }

    pub fn order_update(&mut self, ts_ms: u64, u: &OrderUpdate) {
        self.journal.order_update(ts_ms, u);
    }

    pub fn trade(&mut self, t: &TradeRecord) {
        self.journal.trade(t);
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for a Unix time in milliseconds (UTC).
pub fn fmt_utc_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (y, mo, d) = crate::replay::civil_from_days((secs / 86_400) as i64);
    let sod = secs % 86_400;
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60,
        ms % 1000
    )
}

/// `YYYY-MM-DD` of a UTC day number (days since the Unix epoch).
pub fn fmt_utc_day(day: u64) -> String {
    let (y, mo, d) = crate::replay::civil_from_days(day as i64);
    format!("{y:04}-{mo:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(fmt_utc_ms(0), "1970-01-01T00:00:00.000Z");
        // 2026-11-13T08:48:05.123Z
        assert_eq!(fmt_utc_ms(1_794_559_685_123), "2026-11-13T08:48:05.123Z");
        assert_eq!(fmt_utc_day(1_794_559_685_123 / 86_400_000), "2026-11-13");
        // Leap day.
        assert_eq!(fmt_utc_ms(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }
}
