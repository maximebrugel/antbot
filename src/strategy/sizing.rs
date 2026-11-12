//! Position sizing: config notional clamped to the hard cap, converted to a
//! lot-rounded size.
//!
//! Exchange tick/lot rules (5 significant figures, max 6 - szDecimals price
//! decimals, szDecimals size decimals, 10 USD minimum) live in
//! [`crate::hl::rounding`]; `size_for_notional` there is the building block.
//!
//! TODO(step4): strategy-level sizing on top of it.
