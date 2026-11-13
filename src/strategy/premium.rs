//! Premium of the perp vs its oracle, and the backtest's trade arithmetic.
//!
//! # Which price
//!
//! The research backtest (`hl-research/sim/backtest.py`, `run_trades`) uses
//! the exchange's hourly `fundingHistory.premium` series as the signal. For
//! HIP-3 dexes that premium is the **impact mid vs the oracle**:
//!
//! ```text
//! premium = 0.5 * (impact_bid_px + impact_ask_px) / oracle_px - 1
//! ```
//!
//! (`notes_v2.md`: "HIP-3 premium = 0.5*(impact_bid+impact_ask)/oracle - 1").
//! The live `activeAssetCtx` / `metaAndAssetCtxs` `premium` field is the same
//! quantity at a single point in time. Steps 1 and 2 checked this on live data
//! and on the fixtures: `impactPxs` ["104.62","104.6804"] with oracle 104.59
//! gives 0.0005755808, exactly the API value.
//!
//! * `premium_source = "impact"` (default) is that definition, i.e. the
//!   backtest's.
//! * `premium_source = "book_mid"` swaps the impact prices for the l2Book best
//!   bid / ask. It is a fallback, **not** the backtest definition.
//! * The mark price is never used. The oracle is the denominator in both
//!   cases.
//!
//! `fundingHistory.premium` is one value per funding hour: the mean of the
//! exchange's 5 s premium samples over the hour. That hourly mean is the
//! signal the research validated. The bot sees the instantaneous ctx premium
//! about once a second, so `strategy.premium_avg_secs` (default 3600)
//! smooths it with a time-bucketed rolling mean ([`PremiumAverager`]: one
//! sample per 5 s slot, like the exchange's sampling). The signal, the
//! convergence exit and the persistent-premium kill all use the averaged
//! value. Deviation from the backtest: the rolling mean is evaluated
//! continuously, not only at the top of each hour. The mean is only used
//! once the window is [`AVG_MIN_COVERAGE`] covered (warm-up); the samples
//! are persisted so a restart does not reset it. 0 uses the instantaneous
//! value (**not** the backtested signal).
//!
//! # Trade arithmetic (backtest.py)
//!
//! * side = −sign(premium): premium > 0 → short, < 0 → long ([`fade_side`]).
//! * "basis" return, with the oracle assumed constant:
//!   `side * ((1 + p_exit) / (1 + p_entry) - 1)` ([`basis_return`]).
//! * full mark-to-mark return: `side * (px_exit / px_entry - 1)`
//!   ([`full_return`]).
//! * hit (the writeup's "moved toward 0" rate, `measure_structural.py`): `|p_exit| < |p_entry|`
//!   ([`moved_toward_zero`]).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Position direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    /// +1 for long, −1 for short (backtest `side`).
    pub fn sign(self) -> f64 {
        match self {
            Side::Long => 1.0,
            Side::Short => -1.0,
        }
    }

    /// Whether the order that opens this side buys.
    pub fn open_is_buy(self) -> bool {
        self == Side::Long
    }

    /// Whether the order that closes this side buys.
    pub fn close_is_buy(self) -> bool {
        self == Side::Short
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Side::Long => "long",
            Side::Short => "short",
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Premium of a reference mid vs the oracle, as a fraction. `None` if any
/// input is non-finite or non-positive, or if bid > ask.
pub fn premium_from_bid_ask(bid: f64, ask: f64, oracle: f64) -> Option<f64> {
    let ok = |v: f64| v.is_finite() && v > 0.0;
    if !(ok(bid) && ok(ask) && ok(oracle)) || bid > ask {
        return None;
    }
    Some(0.5 * (bid + ask) / oracle - 1.0)
}

/// Fraction -> basis points.
pub fn to_bps(frac: f64) -> f64 {
    frac * 1e4
}

/// Basis points -> fraction.
pub fn from_bps(bps: f64) -> f64 {
    bps * 1e-4
}

/// Side that profits from the premium converging to 0: premium > 0 → short,
/// premium < 0 → long, exactly 0 (or NaN) → none.
pub fn fade_side(premium: f64) -> Option<Side> {
    if premium > 0.0 {
        Some(Side::Short)
    } else if premium < 0.0 {
        Some(Side::Long)
    } else {
        None
    }
}

/// Entry rule: `|premium_bps| >= threshold_bps`, faded. Returns the side.
pub fn entry_signal(premium_bps: f64, threshold_bps: f64) -> Option<Side> {
    if premium_bps.is_finite() && premium_bps.abs() >= threshold_bps {
        fade_side(premium_bps)
    } else {
        None
    }
}

/// Premium seen from the trade's side: positive while the dislocation we
/// fade is still there, ≤ 0 once it has fully reverted or overshot.
/// (Short entered on premium > 0 → `premium`; long → `-premium`.)
pub fn side_adjusted(side: Side, premium: f64) -> f64 {
    -side.sign() * premium
}

/// Convergence exit rule: the premium has come back to within `exit_bps` of
/// zero on the side we faded, or has crossed to the other side.
/// `side_adjusted(side, p) <= exit_bps`. That is `|p| <= exit_bps`, extended
/// to overshoots so that a jump from +60 to −30 bps between two samples also
/// counts.
pub fn converged(side: Side, premium_bps: f64, exit_bps: f64) -> bool {
    premium_bps.is_finite() && side_adjusted(side, premium_bps) <= exit_bps
}

/// backtest.py `basis = side * ((1 + p_x) / (1 + p_e) - 1)` (fractions).
pub fn basis_return(side: Side, p_entry: f64, p_exit: f64) -> f64 {
    side.sign() * ((1.0 + p_exit) / (1.0 + p_entry) - 1.0)
}

/// backtest.py `full = side * (Px / Pe - 1)`.
pub fn full_return(side: Side, px_entry: f64, px_exit: f64) -> f64 {
    side.sign() * (px_exit / px_entry - 1.0)
}

/// Writeup / measure_structural.py hit definition: `|p_exit| < |p_entry|`.
pub fn moved_toward_zero(p_entry: f64, p_exit: f64) -> bool {
    p_exit.abs() < p_entry.abs()
}

/// Width of a sampling slot (HL samples the premium every 5 s for funding).
pub const PREMIUM_SLOT_MS: u64 = 5_000;

/// Fraction of the window's 5 s slots that must hold a sample before the
/// rolling mean is trusted.
pub const AVG_MIN_COVERAGE: f64 = 0.8;

/// Rolling mean of the premium over a time window, one sample per 5 s slot
/// (the latest value in a slot wins). Gaps are simply not sampled. A window
/// of 0 passes values through.
#[derive(Debug, Clone, Default)]
pub struct PremiumAverager {
    window_ms: u64,
    samples: VecDeque<(u64, f64)>,
}

impl PremiumAverager {
    pub fn new(window_ms: u64) -> Self {
        Self {
            window_ms,
            samples: VecDeque::new(),
        }
    }

    pub fn window_ms(&self) -> u64 {
        self.window_ms
    }

    /// Add a sample taken at `now_ms` and return the current mean.
    pub fn push(&mut self, now_ms: u64, premium: f64) -> f64 {
        if self.window_ms == 0 || !premium.is_finite() {
            return premium;
        }
        let slot = now_ms / PREMIUM_SLOT_MS;
        match self.samples.back_mut() {
            Some((s, v)) if *s == slot => *v = premium,
            _ => self.samples.push_back((slot, premium)),
        }
        self.evict(now_ms);
        self.mean().unwrap_or(premium)
    }

    fn evict(&mut self, now_ms: u64) {
        let min_slot = now_ms.saturating_sub(self.window_ms) / PREMIUM_SLOT_MS;
        while self.samples.front().is_some_and(|(s, _)| *s < min_slot) {
            self.samples.pop_front();
        }
    }

    /// Mean of the samples currently in the window.
    pub fn mean(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        Some(self.samples.iter().map(|(_, v)| v).sum::<f64>() / self.samples.len() as f64)
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Enough of the window is sampled for the mean to stand for it
    /// (always true without averaging).
    pub fn ready(&self) -> bool {
        if self.window_ms == 0 {
            return true;
        }
        let slots = (self.window_ms / PREMIUM_SLOT_MS).max(1) as f64;
        self.samples.len() as f64 >= AVG_MIN_COVERAGE * slots
    }

    /// Samples as (slot start ms, premium), for persistence.
    pub fn samples(&self) -> Vec<(u64, f64)> {
        self.samples
            .iter()
            .map(|(slot, v)| (slot * PREMIUM_SLOT_MS, *v))
            .collect()
    }

    /// Restore persisted samples (those still inside the window at `now_ms`).
    pub fn restore(&mut self, now_ms: u64, samples: &[(u64, f64)]) {
        if self.window_ms == 0 {
            return;
        }
        let mut v: Vec<(u64, f64)> = samples
            .iter()
            .filter(|(t, p)| p.is_finite() && *t <= now_ms)
            .map(|(t, p)| (t / PREMIUM_SLOT_MS, *p))
            .collect();
        v.sort_by_key(|(s, _)| *s);
        v.dedup_by_key(|(s, _)| *s);
        self.samples = v.into_iter().collect();
        self.evict(now_ms);
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_live_snapshot() {
        // metaAndAssetCtxs xyz:ANT, 12 Nov 2026 19:19 Paris:
        // impactPxs ["104.62","104.6804"], oraclePx 104.59, premium 0.0005755808
        let p = premium_from_bid_ask(104.62, 104.6804, 104.59).unwrap();
        assert!((p - 0.000_575_580_8).abs() < 1e-9, "{p}");
        assert!((to_bps(p) - 5.755_808).abs() < 1e-5);
        assert!((from_bps(to_bps(p)) - p).abs() < 1e-18);
    }

    #[test]
    fn impact_mid_not_docs_clamp_formula() {
        // The (non-HIP-3) docs "impact price difference" formula would give
        // max(104.62-104.59,0) - max(104.59-104.6804,0) = 0.03 -> 2.87 bps;
        // the API premium is the impact mid (5.76 bps). Guard against mixing them.
        let p = premium_from_bid_ask(104.62, 104.6804, 104.59).unwrap();
        assert!((to_bps(p) - 2.868).abs() > 1.0);
    }

    #[test]
    fn sign_convention() {
        assert!(premium_from_bid_ask(101.0, 101.0, 100.0).unwrap() > 0.0);
        assert!(premium_from_bid_ask(99.0, 99.0, 100.0).unwrap() < 0.0);
        assert_eq!(premium_from_bid_ask(100.0, 100.0, 100.0).unwrap(), 0.0);
    }

    #[test]
    fn rejects_bad_inputs() {
        assert_eq!(premium_from_bid_ask(0.0, 1.0, 1.0), None);
        assert_eq!(premium_from_bid_ask(1.0, 1.0, 0.0), None);
        assert_eq!(premium_from_bid_ask(f64::NAN, 1.0, 1.0), None);
        assert_eq!(premium_from_bid_ask(2.0, 1.0, 1.0), None);
    }

    #[test]
    fn fade_and_entry_rule() {
        assert_eq!(fade_side(0.01), Some(Side::Short));
        assert_eq!(fade_side(-0.01), Some(Side::Long));
        assert_eq!(fade_side(0.0), None);
        assert_eq!(fade_side(f64::NAN), None);
        assert_eq!(entry_signal(50.0, 50.0), Some(Side::Short)); // >= is inclusive
        assert_eq!(entry_signal(-50.0, 50.0), Some(Side::Long));
        assert_eq!(entry_signal(49.99, 50.0), None);
        assert_eq!(entry_signal(-49.99, 50.0), None);
        assert_eq!(entry_signal(f64::NAN, 50.0), None);
        assert_eq!(entry_signal(f64::INFINITY, 50.0), None);
    }

    #[test]
    fn convergence_rule() {
        // short entered at +60 bps
        assert!(!converged(Side::Short, 60.0, 10.0));
        assert!(!converged(Side::Short, 10.01, 10.0));
        assert!(converged(Side::Short, 10.0, 10.0));
        assert!(converged(Side::Short, 0.0, 10.0));
        assert!(converged(Side::Short, -30.0, 10.0)); // overshoot

        // long entered at -60 bps
        assert!(!converged(Side::Long, -60.0, 10.0));
        assert!(converged(Side::Long, -10.0, 10.0));
        assert!(converged(Side::Long, 25.0, 10.0));
        assert!(!converged(Side::Long, f64::NAN, 10.0));
        assert_eq!(side_adjusted(Side::Short, 60.0), 60.0);
        assert_eq!(side_adjusted(Side::Long, -60.0), 60.0);
    }

    #[test]
    fn backtest_arithmetic_first_ant_trade() {
        // trades.csv S2_ANT 8h_fixed, 2026-10-26T15:00Z long:
        // p_entry -0.0247348947, p_exit 0.0053966419, px 111.95 -> 113.9
        // basis_bps 308.9573946227819, full_bps 174.18490397498988
        let b = basis_return(Side::Long, -0.024_734_894_7, 0.005_396_641_9);
        assert!((to_bps(b) - 308.957_394_622_781_9).abs() < 1e-9, "{b}");
        let f = full_return(Side::Long, 111.95, 113.9);
        assert!((to_bps(f) - 174.184_903_974_989_88).abs() < 1e-9, "{f}");
        assert!(moved_toward_zero(-0.024_734_894_7, 0.005_396_641_9));
        assert!(!moved_toward_zero(0.006, -0.007));
        assert!(!moved_toward_zero(0.006, 0.006)); // strict
    }

    #[test]
    fn side_helpers() {
        assert!(Side::Long.open_is_buy() && !Side::Long.close_is_buy());
        assert!(!Side::Short.open_is_buy() && Side::Short.close_is_buy());
        assert_eq!(Side::Short.to_string(), "short");
        assert_eq!(serde_json::to_string(&Side::Long).unwrap(), "\"long\"");
    }

    #[test]
    fn averager_passthrough_and_window() {
        let mut a = PremiumAverager::new(0);
        assert_eq!(a.push(0, 0.01), 0.01);
        assert!(a.is_empty());

        let mut a = PremiumAverager::new(60_000);
        assert_eq!(a.push(0, 0.006), 0.006);
        // same 5 s slot: replaced, not added
        assert_eq!(a.push(1_000, 0.004), 0.004);
        assert_eq!(a.len(), 1);
        assert!((a.push(5_000, 0.008) - 0.006).abs() < 1e-15);
        assert_eq!(a.len(), 2);
        // 70 s later the first two slots (0 s, 5 s) are out of the 60 s window
        assert!((a.push(70_000, 0.002) - 0.002).abs() < 1e-15);
        assert_eq!(a.len(), 1);
        // non-finite samples are ignored
        assert!(a.push(75_000, f64::NAN).is_nan());
        assert_eq!(a.len(), 1);
        assert_eq!(a.window_ms(), 60_000);
    }

    #[test]
    fn averager_warm_up_and_restore() {
        let mut a = PremiumAverager::new(60_000); // 12 slots, ready at >= 9.6
        for i in 0..9 {
            a.push(i * 5_000, 0.01);
        }
        assert!(!a.ready());
        a.push(45_000, 0.01);
        assert!(a.ready());
        let saved = a.samples();
        let mut b = PremiumAverager::new(60_000);
        b.restore(50_000, &saved);
        assert!(b.ready());
        assert!((b.mean().unwrap() - 0.01).abs() < 1e-15);
        // Restored long after: everything is out of the window.
        let mut c = PremiumAverager::new(60_000);
        c.restore(1_000_000, &saved);
        assert!(c.is_empty() && !c.ready());
        assert!(PremiumAverager::new(0).ready());
    }
}
