//! Alert texts (Telegram and log) and the daily summary.
//!
//! Pure functions so the wording is unit tested. Plain text (no Telegram
//! `parse_mode`), so nothing needs escaping.

use crate::monitor::fmt_utc_day;
use crate::risk::kill::utc_day;
use crate::strategy::premium::Side;
use crate::strategy::signal::{ExitReason, TradeRecord};

/// Telegram hard limit is 4096 characters; keep a margin.
pub const MAX_ALERT_CHARS: usize = 3800;

/// Cut a message to [`MAX_ALERT_CHARS`] characters (on a char boundary).
pub fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_ALERT_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_ALERT_CHARS - 15).collect();
    out.push_str("\n…(truncated)");
    out
}

fn side_up(s: Side) -> &'static str {
    match s {
        Side::Long => "LONG",
        Side::Short => "SHORT",
    }
}

fn bps(v: Option<f64>) -> String {
    v.filter(|x| x.is_finite())
        .map_or_else(|| "n/a".into(), |x| format!("{x:+.1} bps"))
}

fn hold(secs: f64) -> String {
    let m = (secs / 60.0).round() as u64;
    format!("{}h{:02}m", m / 60, m % 60)
}

/// Bot started.
pub fn startup(
    mode: &str,
    coin: &str,
    asset_id: u32,
    notional_usd: f64,
    phase: &str,
    kill: Option<&str>,
) -> String {
    let mut s = format!(
        "🚀 antbot started ({mode})\n{coin} asset {asset_id}, notional {notional_usd:.0} USD\nstate: {phase}"
    );
    if let Some(k) = kill {
        s.push_str(&format!("\n⛔ kill switch active: {k}"));
    }
    s
}

/// Bot stopping.
pub fn shutdown(
    reason: &str,
    phase: &str,
    position: f64,
    realized_usd: f64,
    trades: usize,
) -> String {
    format!(
        "🛑 antbot stopping ({reason})\nstate: {phase}, position {position}\nrealized PnL {realized_usd:+.2} USD over {trades} trades"
    )
}

/// Entry filled (position opened).
pub fn position_opened(
    coin: &str,
    side: Side,
    size: f64,
    avg_px: f64,
    premium_bps: Option<f64>,
) -> String {
    format!(
        "🟢 ENTRY {} {coin}\n{size} @ {avg_px:.4} ({:.0} USD)\npremium at signal: {}",
        side_up(side),
        size * avg_px,
        bps(premium_bps)
    )
}

/// Exit started (converge / deadline / kill).
pub fn exit_started(coin: &str, reason: ExitReason, premium_bps: Option<f64>) -> String {
    format!(
        "↩️ exit started {coin}: {}\npremium {}",
        reason.as_str(),
        bps(premium_bps)
    )
}

/// Taker fallback (IOC) during an exit.
pub fn taker_fallback(coin: &str, remaining: f64) -> String {
    format!("⚠️ taker fallback (IOC) on {coin}: {remaining} left to close")
}

/// Round trip closed.
pub fn trade_closed(coin: &str, t: &TradeRecord) -> String {
    let icon = if t.pnl_usd >= 0.0 { "✅" } else { "❌" };
    format!(
        "{icon} EXIT {} {coin} ({})\n{} {:.4} -> {:.4}, held {}\nPnL {:+.2} USD ({:+.1} bps), fees {:.4}, funding {:+.4}\npremium {} -> {}",
        side_up(t.side),
        t.exit_reason.as_str(),
        t.size,
        t.entry_px,
        t.exit_px,
        hold(t.hold_secs()),
        t.pnl_usd,
        t.pnl_bps,
        t.fees_usd,
        t.funding_usd,
        bps(t.entry_premium_bps),
        bps(t.exit_premium_bps),
    )
}

/// Kill switch fired.
pub fn kill(msg: &str, flatten: bool) -> String {
    format!(
        "⛔ KILL SWITCH: {msg}\n{}\nresume with `antbot reset-kill --confirm` after review",
        if flatten {
            "flattening the position"
        } else {
            "orders canceled, position left untouched"
        }
    )
}

/// Market spec change detected by the meta poll.
pub fn spec_change(changes: &[String], killed: bool) -> String {
    format!(
        "{} market spec change{}:\n{}",
        if killed { "⛔" } else { "⚠️" },
        if killed { " (KILL)" } else { "" },
        changes.join("\n")
    )
}

/// Operator warning (resync, mismatch, rate limit...).
pub fn warning(msg: &str) -> String {
    format!("⚠️ {msg}")
}

/// Error.
pub fn error(msg: &str) -> String {
    format!("🔥 ERROR: {msg}")
}

/// Daily summary (one UTC day).
#[derive(Debug, Clone, PartialEq)]
pub struct DailySummary {
    pub day: u64,
    pub trades: usize,
    pub wins: usize,
    pub pnl_usd: f64,
    pub fees_usd: f64,
    pub funding_usd: f64,
    pub avg_bps: Option<f64>,
    pub realized_total_usd: f64,
    pub equity_usd: f64,
    pub phase: String,
    pub position: f64,
    pub kill: Option<String>,
    pub premium_min_bps: Option<f64>,
    pub premium_max_bps: Option<f64>,
    pub premium_last_bps: Option<f64>,
}

impl DailySummary {
    /// Trades of `trades` closed on UTC day `day`; the rest is filled by
    /// the caller.
    pub fn from_trades(day: u64, trades: &[TradeRecord]) -> Self {
        let today: Vec<&TradeRecord> = trades
            .iter()
            .filter(|t| utc_day(t.closed_ms) == day)
            .collect();
        let n = today.len();
        Self {
            day,
            trades: n,
            wins: today.iter().filter(|t| t.pnl_usd > 0.0).count(),
            // `+ 0.0`: an empty f64 sum is -0.0 (prints "-0.00").
            pnl_usd: today.iter().map(|t| t.pnl_usd).sum::<f64>() + 0.0,
            // `+ 0.0`: an empty f64 sum is -0.0 (prints "-0.00").
            fees_usd: today.iter().map(|t| t.fees_usd).sum::<f64>() + 0.0,
            // `+ 0.0`: an empty f64 sum is -0.0 (prints "-0.00").
            funding_usd: today.iter().map(|t| t.funding_usd).sum::<f64>() + 0.0,
            avg_bps: (n > 0).then(|| today.iter().map(|t| t.pnl_bps).sum::<f64>() / n as f64),
            realized_total_usd: 0.0,
            equity_usd: 0.0,
            phase: String::new(),
            position: 0.0,
            kill: None,
            premium_min_bps: None,
            premium_max_bps: None,
            premium_last_bps: None,
        }
    }

    pub fn text(&self, mode: &str, coin: &str) -> String {
        let mut s = format!(
            "📊 daily summary {} ({mode}, {coin})\ntrades {} (wins {}), PnL {:+.2} USD",
            fmt_utc_day(self.day),
            self.trades,
            self.wins,
            self.pnl_usd
        );
        if let Some(a) = self.avg_bps {
            s.push_str(&format!(", avg {a:+.1} bps"));
        }
        s.push_str(&format!(
            "\nfees {:.4}, funding {:+.4}\nrealized total {:+.2} USD, equity {:+.2} USD\nstate: {}, position {}",
            self.fees_usd,
            self.funding_usd,
            self.realized_total_usd,
            self.equity_usd,
            self.phase,
            self.position
        ));
        if let (Some(lo), Some(hi)) = (self.premium_min_bps, self.premium_max_bps) {
            s.push_str(&format!(
                "\npremium range {lo:+.1} .. {hi:+.1} bps, last {}",
                bps(self.premium_last_bps)
            ));
        }
        if let Some(k) = &self.kill {
            s.push_str(&format!("\n⛔ kill switch active: {k}"));
        }
        s
    }
}

/// Premium range tracker for the current UTC day (signal samples).
/// Persisted in the bot state.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DayTracker {
    pub day: Option<u64>,
    pub min_bps: Option<f64>,
    pub max_bps: Option<f64>,
    pub last_bps: Option<f64>,
}

impl DayTracker {
    pub fn new(now_ms: u64) -> Self {
        Self {
            day: Some(utc_day(now_ms)),
            ..Self::default()
        }
    }

    pub fn sample(&mut self, premium_bps: Option<f64>) {
        let Some(p) = premium_bps.filter(|x| x.is_finite()) else {
            return;
        };
        self.min_bps = Some(self.min_bps.map_or(p, |m| m.min(p)));
        self.max_bps = Some(self.max_bps.map_or(p, |m| m.max(p)));
        self.last_bps = Some(p);
    }

    /// Resume a persisted tracker at startup. Returns the tracker for
    /// today (the persisted one if it is for today, so the premium range
    /// survives a mid-day restart) and, if the persisted day is over, that
    /// finished day: its summary was never sent (the bot was down at
    /// midnight UTC). Days the bot never ran are not reported.
    pub fn resume(saved: Option<DayTracker>, now_ms: u64) -> (DayTracker, Option<DayTracker>) {
        let today = utc_day(now_ms);
        match saved {
            Some(t) if t.day == Some(today) => (t, None),
            Some(t) if t.day.is_some_and(|d| d < today) => (Self::new(now_ms), Some(t)),
            // None, no day, or a day in the future (clock change).
            _ => (Self::new(now_ms), None),
        }
    }

    /// If the UTC day changed, return the finished day (with its premium
    /// range) and start tracking the new one.
    pub fn roll(&mut self, now_ms: u64) -> Option<DayTracker> {
        let today = utc_day(now_ms);
        match self.day {
            Some(d) if d < today => {
                let done = std::mem::replace(self, Self::new(now_ms));
                Some(done)
            }
            None => {
                self.day = Some(today);
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    fn trade(closed_ms: u64, pnl: f64, bps_: f64) -> TradeRecord {
        TradeRecord {
            side: Side::Long,
            size: 24.24,
            entry_px: 103.11,
            exit_px: 103.6,
            signal_ms: closed_ms - 3_000_000,
            opened_ms: closed_ms - 2_700_000,
            closed_ms,
            entry_premium_bps: Some(-62.34),
            exit_premium_bps: Some(-8.0),
            fees_usd: 0.15,
            funding_usd: 0.05,
            gross_pnl_usd: pnl + 0.1,
            pnl_usd: pnl,
            pnl_bps: bps_,
            exit_reason: ExitReason::Converged,
            exit_taker_sz: 0.0,
        }
    }

    #[test]
    fn trade_closed_text() {
        let t = trade(20_000 * DAY, 11.78, 47.1);
        assert_eq!(
            trade_closed("xyz:ANT", &t),
            "✅ EXIT LONG xyz:ANT (converged)\n24.24 103.1100 -> 103.6000, held 0h45m\nPnL +11.78 USD (+47.1 bps), fees 0.1500, funding +0.0500\npremium -62.3 bps -> -8.0 bps"
        );
        let mut l = t.clone();
        l.pnl_usd = -3.0;
        l.entry_premium_bps = None;
        let s = trade_closed("xyz:ANT", &l);
        assert!(s.starts_with("❌ EXIT LONG"));
        assert!(s.contains("PnL -3.00 USD"));
        assert!(s.contains("premium n/a -> -8.0 bps"));
    }

    #[test]
    fn entry_kill_and_lifecycle_texts() {
        assert_eq!(
            position_opened("xyz:ANT", Side::Short, 24.24, 103.11, Some(61.27)),
            "🟢 ENTRY SHORT xyz:ANT\n24.24 @ 103.1100 (2499 USD)\npremium at signal: +61.3 bps"
        );
        assert_eq!(
            kill("hit rate 0.75 < 0.80 over 20 trades", true),
            "⛔ KILL SWITCH: hit rate 0.75 < 0.80 over 20 trades\nflattening the position\nresume with `antbot reset-kill --confirm` after review"
        );
        assert!(kill("x", false).contains("position left untouched"));
        assert_eq!(
            startup("paper", "xyz:ANT", 110138, 2500.0, "flat", None),
            "🚀 antbot started (paper)\nxyz:ANT asset 110138, notional 2500 USD\nstate: flat"
        );
        assert!(startup("paper", "xyz:ANT", 1, 1.0, "halted", Some("dd"))
            .ends_with("⛔ kill switch active: dd"));
        assert_eq!(
            shutdown("SIGTERM", "in_position", -24.24, 12.5, 3),
            "🛑 antbot stopping (SIGTERM)\nstate: in_position, position -24.24\nrealized PnL +12.50 USD over 3 trades"
        );
        assert_eq!(
            spec_change(&["growthMode enabled -> disabled".into()], true),
            "⛔ market spec change (KILL):\ngrowthMode enabled -> disabled"
        );
        assert_eq!(error("boom"), "🔥 ERROR: boom");
        assert_eq!(warning("w"), "⚠️ w");
        assert_eq!(
            exit_started("xyz:ANT", ExitReason::MaxHold, None),
            "↩️ exit started xyz:ANT: max_hold\npremium n/a"
        );
        assert_eq!(
            taker_fallback("xyz:ANT", 3.5),
            "⚠️ taker fallback (IOC) on xyz:ANT: 3.5 left to close"
        );
    }

    #[test]
    fn truncation_respects_limit_and_chars() {
        let long = "é".repeat(5000);
        let t = truncate(&long);
        assert!(t.chars().count() <= MAX_ALERT_CHARS);
        assert!(t.ends_with("(truncated)"));
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn daily_summary_filters_the_day() {
        let d = 20_405u64; // 2025-11-13
        let trades = vec![
            trade(d * DAY - 1, 100.0, 400.0), // previous day
            trade(d * DAY + 1_000, 10.0, 40.0),
            trade(d * DAY + 50_000, -4.0, -16.0),
            trade((d + 1) * DAY, 7.0, 28.0), // next day
        ];
        let mut s = DailySummary::from_trades(d, &trades);
        assert_eq!(s.trades, 2);
        assert_eq!(s.wins, 1);
        assert!((s.pnl_usd - 6.0).abs() < 1e-9);
        assert!((s.avg_bps.unwrap() - 12.0).abs() < 1e-9);
        assert!((s.fees_usd - 0.3).abs() < 1e-9);
        s.realized_total_usd = 113.0;
        s.equity_usd = 110.5;
        s.phase = "flat".into();
        s.premium_min_bps = Some(-72.04);
        s.premium_max_bps = Some(-30.0);
        s.premium_last_bps = Some(-41.0);
        assert_eq!(
            s.text("paper", "xyz:ANT"),
            "📊 daily summary 2025-11-13 (paper, xyz:ANT)\ntrades 2 (wins 1), PnL +6.00 USD, avg +12.0 bps\nfees 0.3000, funding +0.1000\nrealized total +113.00 USD, equity +110.50 USD\nstate: flat, position 0\npremium range -72.0 .. -30.0 bps, last -41.0 bps"
        );
        let empty = DailySummary::from_trades(d, &[]);
        assert_eq!(empty.avg_bps, None);
        assert!(empty
            .text("paper", "xyz:ANT")
            .contains("trades 0 (wins 0), PnL +0.00 USD\n"));
    }

    #[test]
    fn day_tracker_rolls_at_utc_midnight() {
        let t0 = 20_000 * DAY + 3_600_000;
        let mut tr = DayTracker::new(t0);
        tr.sample(Some(-50.0));
        tr.sample(None);
        tr.sample(Some(-70.0));
        tr.sample(Some(-60.0));
        assert!(tr.roll(t0 + 1_000).is_none());
        let done = tr.roll(20_001 * DAY + 5).unwrap();
        assert_eq!(done.day, Some(20_000));
        assert_eq!(done.min_bps, Some(-70.0));
        assert_eq!(done.max_bps, Some(-50.0));
        assert_eq!(done.last_bps, Some(-60.0));
        assert_eq!(tr.day, Some(20_001));
        assert_eq!(tr.min_bps, None);
        assert!(tr.roll(20_001 * DAY + 10).is_none());
    }

    #[test]
    fn day_tracker_resume_after_restart() {
        let mut saved = DayTracker::new(20_000 * DAY + 5);
        saved.sample(Some(-55.0));
        // Same day: keep the range.
        let (t, missed) = DayTracker::resume(Some(saved.clone()), 20_000 * DAY + 9_000_000);
        assert_eq!(t, saved);
        assert!(missed.is_none());
        // Down over midnight: the finished day is reported once.
        let (t, missed) = DayTracker::resume(Some(saved.clone()), 20_001 * DAY + 300_000);
        assert_eq!(t.day, Some(20_001));
        assert_eq!(t.min_bps, None);
        assert_eq!(missed, Some(saved.clone()));
        // Several days down: only the last day the bot ran.
        let (_, missed) = DayTracker::resume(Some(saved.clone()), 20_005 * DAY);
        assert_eq!(missed.unwrap().day, Some(20_000));
        // Fresh state or a clock that went backwards: nothing.
        assert!(DayTracker::resume(None, 20_000 * DAY).1.is_none());
        assert!(DayTracker::resume(Some(saved), 19_999 * DAY).1.is_none());
    }
}
