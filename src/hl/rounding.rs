//! Exchange tick / lot rules for perps (docs: "Tick and lot size"):
//!
//! * price: at most 5 significant figures AND at most `6 - szDecimals`
//!   decimals (integer prices are always allowed);
//! * size: at most `szDecimals` decimals;
//! * minimum order value 10 USD (not for reduce-only closes).
//!
//! [`round_price`] with [`RoundDir::Nearest`] is bit-for-bit the Python SDK
//! formula `round(float(f"{px:.5g}"), 6 - szDecimals)` (checked against
//! SDK-generated values in `tests/signing_vectors.rs`). Maker orders should
//! use `Down` for bids and `Up` for asks so rounding never makes them more
//! aggressive than intended. `Down` / `Up` always snap to 5 significant
//! figures, so prices >= 100,000 are rounded more coarsely than the exchange
//! requires (integers are always valid there). This is intentional and
//! conservative; it does not matter for xyz:ANT (about 100 USD).

use super::signing::float_to_wire;

pub const PERP_MAX_DECIMALS: u32 = 6;
pub const MAX_SIG_FIGS: i32 = 5;
pub const MIN_ORDER_NOTIONAL_USD: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundDir {
    Nearest,
    /// Toward zero / lower price (bids, sizes).
    Down,
    /// Higher price (asks).
    Up,
}

impl RoundDir {
    /// Passive rounding for a maker order on this side.
    pub fn passive(is_buy: bool) -> Self {
        if is_buy {
            RoundDir::Down
        } else {
            RoundDir::Up
        }
    }
    /// Aggressive rounding (taker limit with slippage) on this side.
    pub fn aggressive(is_buy: bool) -> Self {
        if is_buy {
            RoundDir::Up
        } else {
            RoundDir::Down
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum RoundingError {
    #[error("price must be finite and > 0, got {0}")]
    BadPrice(f64),
    #[error("size must be finite and > 0, got {0}")]
    BadSize(f64),
    #[error("size {sz} rounds to zero at {sz_decimals} decimals")]
    SizeRoundsToZero { sz: f64, sz_decimals: u32 },
    #[error("szDecimals {0} exceeds the perp maximum of 6")]
    BadSzDecimals(u32),
}

/// Max price decimals for a perp: `6 - szDecimals`.
pub fn max_price_decimals(sz_decimals: u32) -> u32 {
    PERP_MAX_DECIMALS.saturating_sub(sz_decimals)
}

/// Number of decimals (possibly negative: rounding to tens, hundreds...)
/// allowed for `px` by the 5 significant figure rule and the decimal cap.
fn price_decimals(px: f64, sz_decimals: u32) -> i32 {
    let mag = px.log10().floor() as i32; // 104.57 -> 2
    let sig_dec = MAX_SIG_FIGS - 1 - mag;
    sig_dec.min(max_price_decimals(sz_decimals) as i32)
}

fn fmt_round(x: f64, decimals: i32) -> f64 {
    if decimals >= 0 {
        format!("{x:.prec$}", prec = decimals as usize)
            .parse()
            .expect("formatted float parses")
    } else {
        let f = 10f64.powi(-decimals);
        (x / f).round_ties_even() * f
    }
}

fn snap(x: f64, decimals: i32, dir: RoundDir) -> f64 {
    // Work on x * 10^d with a small tolerance so 104.57 does not become
    // 104.56 because 104.57 * 100 = 10456.999999999998.
    let scale = 10f64.powi(decimals);
    let scaled = x * scale;
    let tol = 1e-10 * scaled.abs().max(1.0);
    let n = match dir {
        RoundDir::Nearest => scaled.round_ties_even(),
        RoundDir::Down => (scaled + tol).floor(),
        RoundDir::Up => (scaled - tol).ceil(),
    };
    fmt_round(n / scale, decimals.max(0))
}

/// Round a perp price to a valid tick.
pub fn round_price(px: f64, sz_decimals: u32, dir: RoundDir) -> Result<f64, RoundingError> {
    if !(px.is_finite() && px > 0.0) {
        return Err(RoundingError::BadPrice(px));
    }
    if sz_decimals > PERP_MAX_DECIMALS {
        return Err(RoundingError::BadSzDecimals(sz_decimals));
    }
    let max_dec = max_price_decimals(sz_decimals) as i32;
    let out = match dir {
        RoundDir::Nearest => {
            // Python: round(float(f"{px:.5g}"), 6 - szd)
            let five: f64 = format!("{px:.prec$e}", prec = (MAX_SIG_FIGS - 1) as usize)
                .parse()
                .expect("formatted float parses");
            fmt_round(five, max_dec)
        }
        _ => snap(px, price_decimals(px, sz_decimals), dir),
    };
    if out.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return Err(RoundingError::BadPrice(px));
    }
    Ok(out)
}

/// Round a size to `szDecimals` (default `Down` so notional never grows).
pub fn round_size(sz: f64, sz_decimals: u32, dir: RoundDir) -> Result<f64, RoundingError> {
    if !(sz.is_finite() && sz > 0.0) {
        return Err(RoundingError::BadSize(sz));
    }
    let out = snap(sz, sz_decimals as i32, dir);
    if out <= 0.0 {
        return Err(RoundingError::SizeRoundsToZero { sz, sz_decimals });
    }
    Ok(out)
}

/// Whether `px` is acceptable as a perp price for this `szDecimals`.
pub fn is_valid_perp_price(px: f64, sz_decimals: u32) -> bool {
    if !(px.is_finite() && px > 0.0) {
        return false;
    }
    let Ok(w) = float_to_wire(px) else {
        return false;
    };
    let Some((int_part, frac)) = w.split_once('.') else {
        return true; // integer prices are always allowed
    };
    if frac.len() as u32 > max_price_decimals(sz_decimals) {
        return false;
    }
    let digits: String = format!("{int_part}{frac}");
    let sig = digits.trim_start_matches('0');
    sig.len() as i32 <= MAX_SIG_FIGS
}

/// Whether `sz` has at most `szDecimals` decimals.
pub fn is_valid_size(sz: f64, sz_decimals: u32) -> bool {
    if !(sz.is_finite() && sz > 0.0) {
        return false;
    }
    match float_to_wire(sz) {
        Ok(w) => w
            .split_once('.')
            .is_none_or(|(_, f)| f.len() as u32 <= sz_decimals),
        Err(_) => false,
    }
}

/// Round `x` to `decimals` decimal places, half away from zero, tolerant
/// of binary floating-point noise: `20.560000000000002` -> `20.56`,
/// `0.1 + 0.2` -> `0.3`, `2.675` (stored as `2.67499999999999982...`) ->
/// `2.68`. The result is the `f64` closest to the decimal value, so it
/// prints without noise. Non-finite values, values too large to carry
/// `decimals` decimals, and `decimals > 15` are returned unchanged; `-0.0`
/// becomes `0.0`.
pub fn round_to_decimals(x: f64, decimals: u32) -> f64 {
    if !x.is_finite() || decimals > 15 {
        return x;
    }
    let k = 10f64.powi(decimals as i32);
    let scaled = x.abs() * k;
    // Already an integer at this scale (or beyond f64 integer precision).
    if !scaled.is_finite() || scaled >= 2f64.powi(52) {
        return x;
    }
    // Tolerance of a few hundred ulps of `scaled` (the noise of a few f64
    // operations is ~1 ulp each), capped far below any meaningful digit,
    // so a decimal half stored a hair under the half still rounds away
    // from zero.
    let tol = (scaled.max(1.0) * 1e-13).min(1e-4);
    let n = (scaled + 0.5 + tol).floor();
    // n is an exact integer and k an exact power of ten (<= 1e15): the
    // division is correctly rounded to the nearest f64 of the decimal.
    (x.signum() * n / k) + 0.0
}

/// Accounting normalisation of a size accumulated from fills (sums and
/// differences of lot-sized quantities): snap it to the `szDecimals` lot
/// grid **only** when it is within float noise of it. Exchange fills are
/// always whole lots, so this removes noise such as `20.560000000000002`
/// without ever moving a size by a meaningful amount; a value genuinely
/// off the grid (never produced by the venue) is returned unchanged.
pub fn snap_size_noise(sz: f64, sz_decimals: u32) -> f64 {
    let r = round_to_decimals(sz, sz_decimals);
    let lot = 10f64.powi(-(sz_decimals.min(15) as i32));
    let tol = (lot * 1e-3).max(sz.abs() * 1e-12);
    if (r - sz).abs() <= tol {
        r
    } else {
        sz
    }
}

/// A size for display (alerts, logs): rounded to `szDecimals` and printed
/// with exactly that many decimals (`20.56`, `24.20`, `3`).
pub fn fmt_size(sz: f64, sz_decimals: u32) -> String {
    let d = sz_decimals.min(15) as usize;
    format!("{:.d$}", round_to_decimals(sz, sz_decimals))
}

/// Size for a USD notional at `px`, rounded down to the lot.
pub fn size_for_notional(
    notional_usd: f64,
    px: f64,
    sz_decimals: u32,
) -> Result<f64, RoundingError> {
    if !(px.is_finite() && px > 0.0) {
        return Err(RoundingError::BadPrice(px));
    }
    round_size(notional_usd / px, sz_decimals, RoundDir::Down)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ant_prices_szd2() {
        // xyz:ANT: szDecimals 2 -> max 4 decimals, but ~100 USD so 5 sig figs -> 2 decimals.
        assert_eq!(
            round_price(104.567891, 2, RoundDir::Nearest).unwrap(),
            104.57
        );
        assert_eq!(round_price(104.567891, 2, RoundDir::Down).unwrap(), 104.56);
        assert_eq!(round_price(104.561, 2, RoundDir::Up).unwrap(), 104.57);
        // Already on tick: unchanged in every direction (float noise tolerated).
        for d in [RoundDir::Nearest, RoundDir::Down, RoundDir::Up] {
            assert_eq!(round_price(104.57, 2, d).unwrap(), 104.57);
            assert_eq!(round_price(103.52, 2, d).unwrap(), 103.52);
            assert_eq!(round_price(0.1 + 0.2, 2, d).unwrap(), 0.3);
        }
        // 9.99995 at 4 decimals max
        assert_eq!(round_price(9.99995, 2, RoundDir::Down).unwrap(), 9.9999);
        assert_eq!(round_price(9.99995, 2, RoundDir::Up).unwrap(), 10.0);
    }

    #[test]
    fn decimal_cap_dominates_for_small_prices() {
        // szd 2 -> 4 decimals even though 5 sig figs would allow more.
        assert_eq!(
            round_price(0.0123456, 2, RoundDir::Nearest).unwrap(),
            0.0123
        );
        assert_eq!(
            round_price(0.0123456, 0, RoundDir::Nearest).unwrap(),
            0.012346
        );
        assert_eq!(round_price(0.0123456, 0, RoundDir::Down).unwrap(), 0.012345);
        assert_eq!(round_price(1.23456, 5, RoundDir::Nearest).unwrap(), 1.2);
        assert_eq!(round_price(1.23456, 5, RoundDir::Up).unwrap(), 1.3);
    }

    #[test]
    fn large_prices() {
        assert_eq!(
            round_price(12345.678, 2, RoundDir::Nearest).unwrap(),
            12346.0
        );
        assert_eq!(round_price(12345.678, 2, RoundDir::Down).unwrap(), 12345.0);
        assert_eq!(
            round_price(123456.78, 2, RoundDir::Nearest).unwrap(),
            123460.0
        );
        assert_eq!(round_price(123456.78, 2, RoundDir::Down).unwrap(), 123450.0);
        assert_eq!(round_price(123451.0, 2, RoundDir::Up).unwrap(), 123460.0);
    }

    #[test]
    fn rounded_prices_are_valid_and_directional() {
        let mut x = 0.000137;
        while x < 2.0e6 {
            for szd in [0, 1, 2, 3, 5] {
                for d in [RoundDir::Nearest, RoundDir::Down, RoundDir::Up] {
                    let tick = 10f64.powi(-(max_price_decimals(szd) as i32));
                    let r = match round_price(x, szd, d) {
                        Ok(r) => r,
                        Err(e) => {
                            // Only possible below one tick (rounds to 0).
                            assert!(x < tick, "{x} szd {szd} {d:?}: {e}");
                            assert_ne!(d, RoundDir::Up);
                            continue;
                        }
                    };
                    assert!(is_valid_perp_price(r, szd), "{x} szd {szd} {d:?} -> {r}");
                    match d {
                        RoundDir::Down => assert!(r <= x * (1.0 + 1e-12), "{x} {r}"),
                        RoundDir::Up => assert!(r >= x * (1.0 - 1e-12), "{x} {r}"),
                        RoundDir::Nearest => {}
                    }
                    // Idempotent
                    assert_eq!(round_price(r, szd, d).unwrap(), r, "{x} {szd} {d:?}");
                }
            }
            x *= 1.0137;
        }
    }

    #[test]
    fn validity_checks() {
        assert!(is_valid_perp_price(104.57, 2));
        assert!(is_valid_perp_price(104.5, 2));
        assert!(!is_valid_perp_price(104.567, 2)); // 6 sig figs
        assert!(!is_valid_perp_price(1.23456, 2)); // 5 decimals > 4
        assert!(is_valid_perp_price(1.2345, 2));
        assert!(is_valid_perp_price(123456.0, 2)); // integer always ok
        assert!(!is_valid_perp_price(0.0, 2));
        assert!(!is_valid_perp_price(-1.0, 2));
        assert!(is_valid_size(23.91, 2));
        assert!(!is_valid_size(23.915, 2));
        assert!(is_valid_size(24.0, 0));
        assert!(!is_valid_size(0.0, 2));
    }

    #[test]
    fn sizes() {
        assert_eq!(
            round_size(23.907430429377452, 2, RoundDir::Down).unwrap(),
            23.9
        );
        assert_eq!(
            round_size(23.907430429377452, 2, RoundDir::Up).unwrap(),
            23.91
        );
        assert_eq!(round_size(23.91, 2, RoundDir::Down).unwrap(), 23.91);
        assert_eq!(round_size(0.29, 2, RoundDir::Down).unwrap(), 0.29); // 0.29*100 = 28.999999999999996
        assert_eq!(
            round_size(0.004, 2, RoundDir::Down),
            Err(RoundingError::SizeRoundsToZero {
                sz: 0.004,
                sz_decimals: 2
            })
        );
        assert!(round_size(f64::NAN, 2, RoundDir::Down).is_err());
        // 2500 USD at 104.57 -> 23.90 ANT (rounded down, never above notional).
        let sz = size_for_notional(2500.0, 104.57, 2).unwrap();
        assert_eq!(sz, 23.9);
        assert!(sz * 104.57 <= 2500.0);
        assert_eq!(float_to_wire(sz).unwrap(), "23.9");
    }

    #[test]
    fn round_to_decimals_removes_float_noise() {
        // The 2026-11-17 exit: three fills summed in f64.
        let sum = 20.19 + 0.35 + 0.02;
        assert_ne!(sum, 20.56); // 20.560000000000002
        assert_eq!(round_to_decimals(sum, 2), 20.56);
        assert_eq!(round_to_decimals(0.1 + 0.2, 1), 0.3);
        assert_eq!(round_to_decimals(0.1 + 0.2, 2), 0.3);
        assert_eq!(round_to_decimals(1.0 - 0.9, 2), 0.1);
        assert_eq!(round_to_decimals(23.907430429377452, 2), 23.91);
        assert_eq!(round_to_decimals(-24.239999999999995, 2), -24.24);
        // Already on the grid: unchanged.
        for x in [0.0, 0.01, 0.29, 24.24, 103.11, 2500.0, -7.5] {
            assert_eq!(round_to_decimals(x, 2), x, "{x}");
        }
    }

    #[test]
    fn round_to_decimals_half_away_from_zero() {
        assert_eq!(round_to_decimals(0.5, 0), 1.0);
        assert_eq!(round_to_decimals(1.5, 0), 2.0);
        assert_eq!(round_to_decimals(2.5, 0), 3.0); // not banker's rounding
        assert_eq!(round_to_decimals(-2.5, 0), -3.0);
        assert_eq!(round_to_decimals(0.125, 2), 0.13);
        assert_eq!(round_to_decimals(-0.125, 2), -0.13);
        // Decimal halves stored just below the half still round away.
        assert_eq!(round_to_decimals(2.675, 2), 2.68);
        assert_eq!(round_to_decimals(1.005, 2), 1.01);
        assert_eq!(round_to_decimals(0.285, 2), 0.29);
        // Not a half: nearest.
        assert_eq!(round_to_decimals(2.674, 2), 2.67);
        assert_eq!(round_to_decimals(2.4, 0), 2.0);
        assert_eq!(round_to_decimals(-2.4, 0), -2.0);
    }

    #[test]
    fn round_to_decimals_edge_cases() {
        // sz_decimals 0: whole units.
        assert_eq!(round_to_decimals(20.000000000000004, 0), 20.0);
        assert_eq!(round_to_decimals(19.999999999999996, 0), 20.0);
        assert_eq!(round_to_decimals(3.49, 0), 3.0);
        // -0.0 and tiny negatives print as 0.
        assert!(round_to_decimals(-0.0, 2).is_sign_positive());
        assert!(round_to_decimals(-0.001, 2).is_sign_positive());
        assert_eq!(round_to_decimals(-0.001, 2), 0.0);
        // Non-finite / huge / too many decimals: unchanged.
        assert!(round_to_decimals(f64::NAN, 2).is_nan());
        assert_eq!(round_to_decimals(f64::INFINITY, 2), f64::INFINITY);
        assert_eq!(round_to_decimals(1e300, 2), 1e300);
        assert_eq!(round_to_decimals(0.123, 16), 0.123);
        // Idempotent on a sweep of values and decimals.
        let mut x = 0.000137;
        while x < 1e7 {
            for d in 0..=6 {
                let r = round_to_decimals(x, d);
                assert_eq!(round_to_decimals(r, d), r, "{x} {d}");
                assert!(
                    (r - x).abs() <= (0.5 + 2e-4) * 10f64.powi(-(d as i32)),
                    "{x} {d}"
                );
            }
            x *= 1.37;
        }
    }

    #[test]
    fn snap_size_noise_only_moves_noise() {
        assert_eq!(snap_size_noise(20.19 + 0.35 + 0.02, 2), 20.56);
        assert_eq!(snap_size_noise(20.56 - 20.19 - 0.35, 2), 0.02);
        assert_eq!(snap_size_noise(20.56 - 20.56, 2), 0.0);
        assert_eq!(snap_size_noise(0.1 + 0.2, 1), 0.3);
        assert_eq!(snap_size_noise(5.000000000000001, 0), 5.0);
        // Genuinely off the lot grid: left alone (never moved by up to
        // half a lot, which could fake a flat position or leave dust).
        assert_eq!(snap_size_noise(0.004, 2), 0.004);
        assert_eq!(snap_size_noise(20.567, 2), 20.567);
        assert_eq!(snap_size_noise(0.4, 0), 0.4);
        // A long run of partial fills accumulates noise but snaps back.
        let mut acc = 0.0;
        for _ in 0..1000 {
            acc += 0.01;
        }
        assert_ne!(acc, 10.0);
        assert_eq!(snap_size_noise(acc, 2), 10.0);
        let mut left = 10.0;
        for _ in 0..999 {
            left -= 0.01;
        }
        assert_eq!(snap_size_noise(left, 2), 0.01);
    }

    #[test]
    fn fmt_size_uses_sz_decimals() {
        assert_eq!(fmt_size(20.19 + 0.35 + 0.02, 2), "20.56");
        assert_eq!(fmt_size(24.2, 2), "24.20");
        assert_eq!(fmt_size(-24.24, 2), "-24.24");
        assert_eq!(fmt_size(0.0, 2), "0.00");
        assert_eq!(fmt_size(-0.0, 2), "0.00");
        assert_eq!(fmt_size(20.000000000000004, 0), "20");
        assert_eq!(fmt_size(3.5, 0), "4");
        assert_eq!(fmt_size(0.1 + 0.2, 3), "0.300");
    }

    #[test]
    fn bad_inputs() {
        assert!(round_price(0.0, 2, RoundDir::Nearest).is_err());
        assert!(round_price(f64::INFINITY, 2, RoundDir::Nearest).is_err());
        assert!(round_price(1.0, 7, RoundDir::Nearest).is_err());
        assert_eq!(max_price_decimals(2), 4);
        assert_eq!(RoundDir::passive(true), RoundDir::Down);
        assert_eq!(RoundDir::passive(false), RoundDir::Up);
        assert_eq!(RoundDir::aggressive(true), RoundDir::Up);
    }
}
