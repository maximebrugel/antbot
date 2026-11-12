//! Position sizing: config notional clamped to the config cap and the
//! built-in 10k USD hard cap (and optionally to the margin available),
//! converted to a lot-rounded contract size. Margin is isolated and leverage
//! defaults to 1x.
//!
//! The exchange's tick and lot rules (5 significant figures, at most
//! 6 - szDecimals price decimals, szDecimals size decimals, 10 USD minimum)
//! live in [`crate::hl::rounding`]. The size is always rounded **down**, so
//! the order notional never exceeds the clamped notional (and so never
//! exceeds the hard cap).

use serde::{Deserialize, Serialize};

use crate::config::{MarginMode, SizingConfig, HARD_CAP_NOTIONAL_USD, MAX_ALLOWED_LEVERAGE};
use crate::hl::rounding::{size_for_notional, RoundingError, MIN_ORDER_NOTIONAL_USD};

/// Result of sizing one entry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SizingPlan {
    /// Notional budget after clamping (USD).
    pub target_notional_usd: f64,
    /// Contract size, rounded down to `szDecimals`.
    pub size: f64,
    /// Reference price used for the conversion.
    pub px: f64,
    /// `size * px`, always <= `target_notional_usd` <= hard cap.
    pub notional_usd: f64,
    /// Leverage to set (isolated).
    pub leverage: u32,
    /// Isolated margin the position needs: `notional_usd / leverage`.
    pub margin_usd: f64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SizingError {
    #[error("margin mode must be isolated")]
    NotIsolated,
    #[error("reference price must be finite and > 0, got {0}")]
    BadPrice(f64),
    #[error("notional {notional_usd:.2} USD is below the exchange minimum of {MIN_ORDER_NOTIONAL_USD} USD")]
    BelowMinimum { notional_usd: f64 },
    #[error("notional {notional_usd:.2} USD exceeds the {HARD_CAP_NOTIONAL_USD} USD hard cap")]
    ExceedsHardCap { notional_usd: f64 },
    #[error("no margin available")]
    NoMargin,
    #[error(transparent)]
    Rounding(#[from] RoundingError),
}

/// Leverage actually used: config value bounded to 1..=min(2, asset max).
pub fn effective_leverage(config_leverage: u32, asset_max_leverage: u32) -> u32 {
    let hi = MAX_ALLOWED_LEVERAGE.min(asset_max_leverage.max(1));
    config_leverage.clamp(1, hi)
}

/// Notional budget: min(config notional, config cap, hard cap), and, when
/// `available_margin_usd` is known, at most `available * leverage`.
pub fn target_notional(
    cfg: &SizingConfig,
    leverage: u32,
    available_margin_usd: Option<f64>,
) -> f64 {
    let mut n = cfg
        .notional_usd
        .min(cfg.max_notional_usd)
        .min(HARD_CAP_NOTIONAL_USD);
    if let Some(avail) = available_margin_usd {
        n = n.min(avail.max(0.0) * leverage as f64);
    }
    if n.is_finite() {
        n.max(0.0)
    } else {
        0.0
    }
}

/// Size an entry at reference price `px`.
pub fn plan_entry(
    cfg: &SizingConfig,
    px: f64,
    sz_decimals: u32,
    asset_max_leverage: u32,
    available_margin_usd: Option<f64>,
) -> Result<SizingPlan, SizingError> {
    if cfg.margin_mode != MarginMode::Isolated {
        return Err(SizingError::NotIsolated);
    }
    if !(px.is_finite() && px > 0.0) {
        return Err(SizingError::BadPrice(px));
    }
    let leverage = effective_leverage(cfg.leverage, asset_max_leverage);
    let target = target_notional(cfg, leverage, available_margin_usd);
    if target <= 0.0 {
        return Err(SizingError::NoMargin);
    }
    if target < MIN_ORDER_NOTIONAL_USD {
        return Err(SizingError::BelowMinimum {
            notional_usd: target,
        });
    }
    let size = size_for_notional(target, px, sz_decimals)?;
    let notional = size * px;
    if notional < MIN_ORDER_NOTIONAL_USD {
        return Err(SizingError::BelowMinimum {
            notional_usd: notional,
        });
    }
    // Rounding down guarantees this; keep a hard assertion for safety.
    if notional > HARD_CAP_NOTIONAL_USD * (1.0 + 1e-9) {
        return Err(SizingError::ExceedsHardCap {
            notional_usd: notional,
        });
    }
    Ok(SizingPlan {
        target_notional_usd: target,
        size,
        px,
        notional_usd: notional,
        leverage,
        margin_usd: notional / leverage as f64,
    })
}

/// Half a lot: sizes below this are treated as zero.
pub fn lot_epsilon(sz_decimals: u32) -> f64 {
    0.5 * 10f64.powi(-(sz_decimals as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(notional: f64, max: f64, lev: u32) -> SizingConfig {
        SizingConfig {
            notional_usd: notional,
            max_notional_usd: max,
            leverage: lev,
            margin_mode: MarginMode::Isolated,
        }
    }

    #[test]
    fn default_ant_sizing() {
        // 2500 USD at 104.57, szDecimals 2 -> 23.90 (23.907... rounded down)
        let p = plan_entry(&SizingConfig::default(), 104.57, 2, 10, None).unwrap();
        assert_eq!(p.size, 23.9);
        assert_eq!(p.leverage, 1);
        assert_eq!(p.target_notional_usd, 2500.0);
        assert!(p.notional_usd <= 2500.0 && p.notional_usd > 2499.0);
        assert!((p.margin_usd - p.notional_usd).abs() < 1e-9);
    }

    #[test]
    fn hard_cap_clamps_even_if_config_bypassed() {
        // Validation would reject these, but sizing clamps anyway.
        let p = plan_entry(&cfg(50_000.0, 50_000.0, 1), 104.0, 2, 10, None).unwrap();
        assert!(p.notional_usd <= HARD_CAP_NOTIONAL_USD);
        assert_eq!(p.target_notional_usd, HARD_CAP_NOTIONAL_USD);
        assert_eq!(p.size, 96.15); // 10000/104 = 96.1538 -> 96.15
        let p = plan_entry(&cfg(8_000.0, 5_000.0, 1), 104.0, 2, 10, None).unwrap();
        assert_eq!(p.target_notional_usd, 5_000.0);
    }

    #[test]
    fn at_cap_rounding_never_exceeds() {
        for px in [99.99, 100.0, 103.33, 104.57, 111.11, 0.333] {
            let p = plan_entry(&cfg(10_000.0, 10_000.0, 1), px, 2, 10, None).unwrap();
            assert!(p.notional_usd <= 10_000.0 + 1e-9, "{px}: {p:?}");
        }
    }

    #[test]
    fn leverage_is_bounded() {
        assert_eq!(effective_leverage(1, 10), 1);
        assert_eq!(effective_leverage(2, 10), 2);
        assert_eq!(effective_leverage(5, 10), 2); // built-in max 2
        assert_eq!(effective_leverage(0, 10), 1);
        assert_eq!(effective_leverage(2, 1), 1); // asset max 1
        assert_eq!(effective_leverage(2, 0), 1);
        let p = plan_entry(&cfg(2_500.0, 10_000.0, 2), 100.0, 2, 10, None).unwrap();
        assert_eq!(p.leverage, 2);
        assert!((p.margin_usd - 1_250.0).abs() < 1e-9);
    }

    #[test]
    fn available_margin_limits_notional() {
        let p = plan_entry(&SizingConfig::default(), 100.0, 2, 10, Some(1_000.0)).unwrap();
        assert_eq!(p.target_notional_usd, 1_000.0);
        assert_eq!(p.size, 10.0);
        let p = plan_entry(&cfg(2_500.0, 10_000.0, 2), 100.0, 2, 10, Some(1_000.0)).unwrap();
        assert_eq!(p.target_notional_usd, 2_000.0);
        assert_eq!(
            plan_entry(&SizingConfig::default(), 100.0, 2, 10, Some(0.0)),
            Err(SizingError::NoMargin)
        );
        assert!(matches!(
            plan_entry(&SizingConfig::default(), 100.0, 2, 10, Some(5.0)),
            Err(SizingError::BelowMinimum { .. })
        ));
    }

    #[test]
    fn rejects_cross_bad_price_and_dust() {
        let c = SizingConfig {
            margin_mode: MarginMode::Cross,
            ..Default::default()
        };
        assert_eq!(
            plan_entry(&c, 100.0, 2, 10, None),
            Err(SizingError::NotIsolated)
        );
        for px in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                plan_entry(&SizingConfig::default(), px, 2, 10, None),
                Err(SizingError::BadPrice(_))
            ));
        }
        // 12 USD at 100 with szDecimals 0 -> size 0 -> rounding error
        assert!(matches!(
            plan_entry(&cfg(12.0, 100.0, 1), 100.0, 0, 10, None),
            Err(SizingError::Rounding(_))
        ));
        // 15 USD at 9.0, szd 0 -> 1 contract = 9 USD < 10 minimum
        assert!(matches!(
            plan_entry(&cfg(15.0, 100.0, 1), 9.0, 0, 10, None),
            Err(SizingError::BelowMinimum { .. })
        ));
    }

    #[test]
    fn lot_eps() {
        assert_eq!(lot_epsilon(2), 0.005);
        assert_eq!(lot_epsilon(0), 0.5);
    }
}
