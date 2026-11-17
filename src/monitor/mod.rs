//! Monitoring: tracing setup, CSV journals, Telegram alerts, daily summary.
//!
//! * [`logging`]: stdout + daily rolling file in `<data_dir>/logs/`,
//!   `RUST_LOG` filter, optional JSON;
//! * [`csv`]: `signals.csv`, `orders.csv`, `trades.csv` in the data dir
//!   (header once, appended across restarts);
//! * [`telegram`]: non-blocking, rate-limited alert queue (disabled with a
//!   log line when `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` are unset);
//! * [`alerts`]: the alert texts (pure functions, unit tested) and the
//!   daily summary;
//! * [`commands`]: read-only inbound Telegram commands (`/pnl`, `/trades`,
//!   `/help`) answered from a state snapshot published by the main loop;
//! * [`health`]: optional loopback `/healthz` + `/metrics` HTTP server and
//!   the `antbot healthcheck` probe.
//!
//! Exit alerts are coalesced ([`ExitCoalescer`]): "exit started" is held
//! for `[monitor].telegram_exit_coalesce_secs` and, when the trade closes
//! within that window, sent together with the trade-closed alert as one
//! message, so a single Telegram failure cannot lose the start alert on
//! its own. A held alert is flushed when its window ends, before a taker
//! fallback alert, at shutdown and when the [`Monitor`] is dropped.

pub mod alerts;
pub mod commands;
pub mod csv;
pub mod health;
pub mod logging;
pub mod telegram;

use std::collections::HashMap;

use crate::strategy::signal::{Command, ExitReason, OrderUpdate, TradeRecord};

/// Minimum gap between two Telegram alerts of the same kind sent through
/// [`Monitor::alert_throttled`] (the log line is never throttled).
pub const ALERT_REPEAT_GAP_MS: u64 = 15 * 60 * 1000;

/// Default `[monitor].telegram_exit_coalesce_secs`: how long "exit
/// started" is held, waiting for the trade-closed alert to merge with.
pub const DEFAULT_EXIT_COALESCE_SECS: u64 = 10;

/// An "exit started" alert waiting for its trade-closed alert.
#[derive(Debug, Clone, PartialEq)]
struct HeldExit {
    at_ms: u64,
    reason: ExitReason,
    premium_bps: Option<f64>,
    /// The standalone text, sent if no close arrives in time.
    text: String,
}

/// Merges "exit started" and "trade closed" into one alert when the close
/// follows within `window_ms` (pure: returns the texts to send now).
///
/// There is at most one position, so a held start always belongs to the
/// position the next close closes. A held start is never dropped: it is
/// returned by [`Self::due`] once the window is over (or the clock went
/// backwards), by [`Self::take`], or before a newer start / a close that
/// arrives too late.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExitCoalescer {
    /// 0 = disabled: every alert is returned at once.
    window_ms: u64,
    held: Option<HeldExit>,
}

impl ExitCoalescer {
    pub fn new(window_ms: u64) -> Self {
        Self {
            window_ms,
            held: None,
        }
    }

    pub fn is_holding(&self) -> bool {
        self.held.is_some()
    }

    /// Exit started: held (nothing to send yet), or returned at once when
    /// coalescing is disabled. A previous held start is released first.
    pub fn exit_started(
        &mut self,
        now_ms: u64,
        coin: &str,
        reason: ExitReason,
        premium_bps: Option<f64>,
    ) -> Vec<String> {
        let text = alerts::exit_started(coin, reason, premium_bps);
        let mut out: Vec<String> = self.take().into_iter().collect();
        if self.window_ms == 0 {
            out.push(text);
        } else {
            self.held = Some(HeldExit {
                at_ms: now_ms,
                reason,
                premium_bps,
                text,
            });
        }
        out
    }

    /// Trade closed (`closed` is its alert text): one combined message
    /// when a start is held within the window, else the held start (if
    /// any, too old) followed by the close.
    pub fn trade_closed(&mut self, now_ms: u64, closed: String) -> Vec<String> {
        match self.held.take() {
            Some(h) if self.within(&h, now_ms) => vec![alerts::exit_started_and_closed(
                &closed,
                h.reason,
                h.premium_bps,
                now_ms.saturating_sub(h.at_ms) / 1000,
            )],
            Some(h) => vec![h.text, closed],
            None => vec![closed],
        }
    }

    /// The held start once its window is over (call it regularly).
    pub fn due(&mut self, now_ms: u64) -> Option<String> {
        if self.held.as_ref().is_some_and(|h| !self.within(h, now_ms)) {
            return self.take();
        }
        None
    }

    /// The held start, whatever its age (shutdown, drop, ordering).
    pub fn take(&mut self) -> Option<String> {
        self.held.take().map(|h| h.text)
    }

    /// Still inside the window. A clock that went backwards ends it (the
    /// start must not be held for as long as the clock step).
    fn within(&self, h: &HeldExit, now_ms: u64) -> bool {
        now_ms >= h.at_ms && now_ms - h.at_ms < self.window_ms
    }
}

/// What the main loop talks to: CSV journals + alert queue.
#[derive(Debug, Default)]
pub struct Monitor {
    pub journal: csv::Journal,
    pub alerter: telegram::Alerter,
    pub coin: String,
    /// Size decimals of the traded asset (sizes in alerts).
    pub sz_decimals: u32,
    /// Last Telegram send per alert kind (see [`alert_kind`]).
    last_alert_ms: HashMap<String, u64>,
    /// Exit-started / trade-closed merging (disabled by default).
    exits: ExitCoalescer,
}

impl Drop for Monitor {
    /// A held "exit started" is queued rather than lost, whatever path
    /// ends the run (the runner flushes the alert queue after this).
    fn drop(&mut self) {
        self.flush_held_exit();
    }
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
            sz_decimals: 0,
            last_alert_ms: HashMap::new(),
            exits: ExitCoalescer::default(),
        }
    }

    /// Sizes in alerts are printed with `sz_decimals` decimals.
    pub fn with_sz_decimals(mut self, sz_decimals: u32) -> Self {
        self.sz_decimals = sz_decimals;
        self
    }

    /// Hold "exit started" up to `window_ms` to merge it with the
    /// trade-closed alert (0 = never hold).
    pub fn with_exit_coalesce_ms(mut self, window_ms: u64) -> Self {
        self.exits = ExitCoalescer::new(window_ms);
        self
    }

    fn send_all(&self, texts: impl IntoIterator<Item = String>) {
        for t in texts {
            self.alerter.send(t);
        }
    }

    /// "Exit started" (Telegram only; held, see [`ExitCoalescer`]).
    pub fn exit_started(&mut self, now_ms: u64, reason: ExitReason, premium_bps: Option<f64>) {
        let out = self
            .exits
            .exit_started(now_ms, &self.coin, reason, premium_bps);
        self.send_all(out);
    }

    /// "Trade closed" (Telegram only), merged with a held "exit started".
    pub fn trade_closed(&mut self, now_ms: u64, t: &TradeRecord) {
        let closed = alerts::trade_closed(&self.coin, t, self.sz_decimals);
        let out = self.exits.trade_closed(now_ms, closed);
        self.send_all(out);
    }

    /// Taker fallback (Telegram only), after a held "exit started" so the
    /// two arrive in order.
    pub fn taker_fallback(&mut self, remaining: f64) {
        self.flush_held_exit();
        self.alerter.send(alerts::taker_fallback(
            &self.coin,
            remaining,
            self.sz_decimals,
        ));
    }

    /// Send a held "exit started" whose window is over. Call regularly.
    pub fn flush_due(&mut self, now_ms: u64) {
        if let Some(t) = self.exits.due(now_ms) {
            self.alerter.send(t);
        }
    }

    /// Send a held "exit started" now (shutdown).
    pub fn flush_held_exit(&mut self) {
        if let Some(t) = self.exits.take() {
            self.alerter.send(t);
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

    use crate::strategy::premium::Side;

    const COIN: &str = "xyz:ANT";

    fn record() -> TradeRecord {
        TradeRecord {
            side: Side::Long,
            size: 20.19 + 0.35 + 0.02,
            entry_px: 103.11,
            exit_px: 103.6,
            signal_ms: 0,
            opened_ms: 1_000,
            closed_ms: 2_701_000,
            entry_premium_bps: Some(-62.34),
            exit_premium_bps: Some(-8.0),
            fees_usd: 0.15,
            funding_usd: 0.05,
            gross_pnl_usd: 10.1,
            pnl_usd: 10.0,
            pnl_bps: 47.1,
            exit_reason: ExitReason::Converged,
            exit_taker_sz: 0.0,
        }
    }

    fn closed_text() -> String {
        alerts::trade_closed(COIN, &record(), 2)
    }

    #[test]
    fn exit_alerts_within_the_window_make_one_message() {
        let mut c = ExitCoalescer::new(10_000);
        let t0 = 1_000_000;
        assert!(c
            .exit_started(t0, COIN, ExitReason::Converged, Some(-8.3))
            .is_empty());
        assert!(c.is_holding());
        assert_eq!(c.due(t0 + 9_999), None);
        let out = c.trade_closed(t0 + 3_400, closed_text());
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0],
            "✅ EXIT LONG xyz:ANT (converged)\n20.56 103.1100 -> 103.6000, held 0h45m\nPnL +10.00 USD (+47.1 bps), fees 0.1500, funding +0.0500\npremium -62.3 bps -> -8.0 bps\n↩️ exit started 3s before: converged, premium -8.3 bps"
        );
        assert!(!c.is_holding());
        // Nothing left to flush.
        assert_eq!(c.due(t0 + 60_000), None);
        assert_eq!(c.take(), None);
    }

    #[test]
    fn exit_alerts_outside_the_window_are_two_messages() {
        let mut c = ExitCoalescer::new(10_000);
        let t0 = 1_000_000;
        c.exit_started(t0, COIN, ExitReason::MaxHold, None);
        // The loop flushes the start when the window ends...
        assert_eq!(
            c.due(t0 + 10_000).as_deref(),
            Some("↩️ exit started xyz:ANT: max_hold\npremium n/a")
        );
        // ...and the close comes alone later.
        assert_eq!(
            c.trade_closed(t0 + 600_000, closed_text()),
            vec![closed_text()]
        );
        // A close arriving after the window before any flush: the start
        // first, then the close (two alerts, in order).
        c.exit_started(t0, COIN, ExitReason::MaxHold, None);
        let out = c.trade_closed(t0 + 10_001, closed_text());
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with("↩️ exit started"));
        assert_eq!(out[1], closed_text());
    }

    #[test]
    fn held_exit_started_is_never_lost() {
        let t0 = 1_000_000;
        // No close: flushed by due() once the window is over.
        let mut c = ExitCoalescer::new(10_000);
        c.exit_started(t0, COIN, ExitReason::Kill, Some(12.0));
        assert_eq!(c.due(t0 + 5_000), None);
        assert!(c
            .due(t0 + 10_000)
            .unwrap()
            .contains("exit started xyz:ANT: kill"));
        // Shutdown inside the window: take() returns it.
        c.exit_started(t0, COIN, ExitReason::Kill, None);
        assert!(c.take().unwrap().contains("exit started"));
        // A clock stepping backwards ends the hold.
        c.exit_started(t0, COIN, ExitReason::Kill, None);
        assert!(c.due(t0 - 500_000).is_some());
        // A second start releases the first one.
        c.exit_started(t0, COIN, ExitReason::Converged, None);
        let out = c.exit_started(t0 + 1, COIN, ExitReason::Kill, None);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("converged"));
        assert!(c.take().unwrap().contains("kill"));
    }

    #[test]
    fn coalescing_disabled_sends_at_once() {
        let mut c = ExitCoalescer::new(0);
        let out = c.exit_started(5, COIN, ExitReason::Converged, None);
        assert_eq!(
            out,
            vec![alerts::exit_started(COIN, ExitReason::Converged, None)]
        );
        assert!(!c.is_holding());
        assert_eq!(c.trade_closed(6, closed_text()), vec![closed_text()]);
    }

    #[test]
    fn monitor_routes_exit_alerts_through_the_coalescer() {
        let mut m = Monitor::disabled()
            .with_sz_decimals(2)
            .with_exit_coalesce_ms(10_000);
        m.exit_started(1_000, ExitReason::Converged, Some(-8.0));
        assert!(m.exits.is_holding());
        m.flush_due(5_000);
        assert!(m.exits.is_holding());
        m.trade_closed(6_000, &record());
        assert!(!m.exits.is_holding());
        m.exit_started(20_000, ExitReason::MaxHold, None);
        m.taker_fallback(20.56);
        assert!(!m.exits.is_holding(), "flushed before the taker fallback");
        m.exit_started(30_000, ExitReason::MaxHold, None);
        m.flush_held_exit();
        assert!(!m.exits.is_holding());
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
