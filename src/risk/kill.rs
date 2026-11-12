//! Kill rules: rolling hit rate < min over last N trades, persistent
//! |premium| above a threshold, spec change (multiplier / oracle / halt /
//! growthMode / fees / szDecimals / maxLeverage), daily loss limit, max drawdown.
//! Kill state is persisted and requires a manual reset.
//!
//! TODO(step4).
