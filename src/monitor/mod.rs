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

use std::collections::HashMap;

use crate::strategy::signal::{Command, OrderUpdate, TradeRecord};

/// Minimum gap between two Telegram alerts of the same kind sent through
/// [`Monitor::alert_throttled`] (the log line is never throttled).
pub const ALERT_REPEAT_GAP_MS: u64 = 15 * 60 * 1000;

/// What the main loop talks to: CSV journals + alert queue.
#[derive(Debug, Default)]
pub struct Monitor {
    pub journal: csv::Journal,
    pub alerter: telegram::Alerter,
    pub coin: String,
    /// Last Telegram send per alert kind (see [`alert_kind`]).
    last_alert_ms: HashMap<String, u64>,
}

/// An alert's kind: its text with every number replaced by `#`, so
/// repeats of the same warning with other sizes / prices match.
pub fn alert_kind(msg: &str) -> String {
    let mut k = String::with_capacity(msg.len());
    let mut in_num = false;
    for c in msg.chars() {
        let num = c.is_ascii_digit() || (in_num && (c == '.' || c == 'e' || c == '-'));
        if num {
            if !in_num {
                k.push('#');
            }
        } else {
            k.push(c);
        }
        in_num = num;
    }
    k
}

impl Monitor {
    pub fn new(journal: csv::Journal, alerter: telegram::Alerter, coin: &str) -> Self {
        Self {
            journal,
            alerter,
            coin: coin.to_string(),
            last_alert_ms: HashMap::new(),
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

    /// Like [`Self::alert`], but the Telegram part is sent at most once
    /// per [`ALERT_REPEAT_GAP_MS`] for the same kind of message (warnings
    /// that can repeat every minute). Always logged.
    pub fn alert_throttled(&mut self, now_ms: u64, msg: &str) {
        tracing::warn!(alert = true, "{msg}");
        let kind = alert_kind(msg);
        let due = self
            .last_alert_ms
            .get(&kind)
            .is_none_or(|t| now_ms.saturating_sub(*t) >= ALERT_REPEAT_GAP_MS);
        if due {
            if self.last_alert_ms.len() >= 256 {
                self.last_alert_ms
                    .retain(|_, t| now_ms.saturating_sub(*t) < ALERT_REPEAT_GAP_MS);
            }
            self.last_alert_ms.insert(kind, now_ms);
            self.alerter.send(alerts::warning(msg));
        }
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

    pub fn command(&mut self, ts_ms: u64, c: &Command, touch: (Option<f64>, Option<f64>)) {
        self.journal.command(ts_ms, c, touch);
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
    fn alert_kinds_ignore_numbers() {
        assert_eq!(
            alert_kind("entry sizing failed: margin 12.5 < 2500"),
            "entry sizing failed: margin # < #"
        );
        assert_eq!(
            alert_kind("fill for unknown order Some(\"e7\"): buy 3.2e-5 @ 103.1"),
            alert_kind("fill for unknown order Some(\"e12\"): buy 4 @ 99")
        );
        assert_ne!(alert_kind("a 1"), alert_kind("b 1"));
    }

    #[test]
    fn throttled_alerts_are_rate_limited_per_kind() {
        let mut m = Monitor::disabled();
        let t0 = 1_000_000;
        m.alert_throttled(t0, "entry sizing failed: 1");
        assert_eq!(m.last_alert_ms.get("entry sizing failed: #"), Some(&t0));
        m.alert_throttled(t0 + 60_000, "entry sizing failed: 2");
        assert_eq!(m.last_alert_ms.get("entry sizing failed: #"), Some(&t0));
        m.alert_throttled(t0 + 60_000, "closing fill 3 exceeds open size 2");
        assert_eq!(m.last_alert_ms.len(), 2);
        m.alert_throttled(t0 + ALERT_REPEAT_GAP_MS, "entry sizing failed: 3");
        assert_eq!(
            m.last_alert_ms.get("entry sizing failed: #"),
            Some(&(t0 + ALERT_REPEAT_GAP_MS))
        );
    }

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
