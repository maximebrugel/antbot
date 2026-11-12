//! Strategy: premium computation ([`premium`]), signal state machine
//! ([`signal`]) and position sizing ([`sizing`]). Everything here is pure and
//! deterministic: time and market data are inputs, orders are outputs.

pub mod premium;
pub mod signal;
pub mod sizing;

pub use premium::Side;
pub use signal::{Strategy, StrategyParams};
