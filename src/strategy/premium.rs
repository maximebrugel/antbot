//! Premium of the perp vs its oracle.
//!
//! HL funding docs / research (`notes_v2.md`, `backtest.py` uses
//! `fundingHistory.premium`):
//!   premium = 0.5 * (impact_bid_px + impact_ask_px) / oracle_px - 1
//!
//! `book_mid` variant replaces the impact prices by the best bid / ask.
//! Step 4 builds the signal logic on top of these pure helpers.

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
}
