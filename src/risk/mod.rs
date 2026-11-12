//! Risk management: kill-switch rules, watched market spec, persisted state.

pub mod kill;
pub mod spec;

pub use kill::{KillOrder, KillReason, KillRecord, RiskManager, RiskParams, RiskState};
pub use spec::WatchedSpec;
