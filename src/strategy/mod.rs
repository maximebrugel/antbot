//! Strategy: premium computation ([`premium`]), signal state machine
//! ([`signal`]) and position sizing ([`sizing`]). Everything here is pure and
//! deterministic: time and market data are inputs, orders are outputs.
//!
//! [`TradingStrategy`] is the interface the shared stack (the
//! [`crate::engine::Engine`], the runner's main loop, the paper / live
//! executors, the risk manager, journals, alerts, Telegram commands and the
//! health server) uses to drive a strategy. [`Strategy`] (premium
//! mean-reversion) is the implementation shipped today; another strategy
//! implements the same trait, speaks the same order vocabulary
//! ([`signal::Command`], [`signal::OrderUpdate`], [`signal::Step`]) and
//! reuses everything else unchanged.

pub mod premium;
pub mod signal;
pub mod sizing;

use serde::de::DeserializeOwned;
use serde::Serialize;

pub use premium::Side;
pub use signal::{Strategy, StrategyParams};

use signal::{OrderUpdate, Phase, PositionBook, Step, TickInput, WorkingOrder};

/// What the engine, runner and monitoring need from a strategy.
///
/// Contract (the same as [`Strategy`]'s):
/// * pure and synchronous: time and data come in as arguments, orders go
///   out as [`signal::Command`]s inside a [`Step`] (with [`signal::Event`]s
///   for logs / alerts / the ledger); the executor's feedback comes back
///   through [`TradingStrategy::on_order_update`];
/// * at most one working order, identified by its tag
///   ([`TradingStrategy::working`]), and one position
///   ([`TradingStrategy::position`]);
/// * entries only when [`TickInput::entries_allowed`] (the risk manager's
///   verdict); [`TradingStrategy::on_kill`] halts (optionally flattening)
///   until [`TradingStrategy::resume`];
/// * [`TradingStrategy::state`] is everything needed to resume after a
///   restart; it is persisted as the `strategy` field of
///   `state_<mode>.json`.
pub trait TradingStrategy {
    /// Persisted state (`state_<mode>.json` -> `strategy`).
    type State: Clone + std::fmt::Debug + PartialEq + Serialize + DeserializeOwned;

    /// Short identifier for logs and monitoring.
    fn name(&self) -> &'static str;
    /// State to persist.
    fn state(&self) -> &Self::State;
    /// Current phase (flat, entering, in position, exiting, halted, ...).
    fn phase(&self) -> &Phase;
    /// Open position, if any.
    fn position(&self) -> Option<&PositionBook>;
    /// The order the strategy is waiting on, if any.
    fn working(&self) -> Option<&WorkingOrder>;
    /// Market tick (also called every second for time-based rules).
    fn on_tick(&mut self, input: TickInput) -> Step;
    /// Order feedback from the executor.
    fn on_order_update(&mut self, now_ms: u64, upd: OrderUpdate) -> Step;
    /// Funding paid (< 0) or received (> 0) on the open position.
    fn on_funding(&mut self, usd: f64);
    /// Kill switch: halt, cancel the working order, optionally flatten.
    fn on_kill(&mut self, now_ms: u64, flatten: bool, reason: &str) -> Step;
    /// The kill was cleared (`reset-kill`): leave the halted phase.
    fn resume(&mut self, now_ms: u64) -> Step;
    /// Graceful shutdown: record feedback, issue nothing.
    fn suspend(&mut self);
    /// Take over a position found on the venue at startup.
    fn adopt_position(
        &mut self,
        side: Side,
        size: f64,
        entry_px: f64,
        opened_ms: u64,
    ) -> Result<(), String>;
}

impl TradingStrategy for Strategy {
    type State = signal::StrategyState;

    fn name(&self) -> &'static str {
        "premium_mean_reversion"
    }
    fn state(&self) -> &Self::State {
        Strategy::state(self)
    }
    fn phase(&self) -> &Phase {
        Strategy::phase(self)
    }
    fn position(&self) -> Option<&PositionBook> {
        Strategy::position(self)
    }
    fn working(&self) -> Option<&WorkingOrder> {
        Strategy::working(self)
    }
    fn on_tick(&mut self, input: TickInput) -> Step {
        Strategy::on_tick(self, input)
    }
    fn on_order_update(&mut self, now_ms: u64, upd: OrderUpdate) -> Step {
        Strategy::on_order_update(self, now_ms, upd)
    }
    fn on_funding(&mut self, usd: f64) {
        Strategy::on_funding(self, usd)
    }
    fn on_kill(&mut self, now_ms: u64, flatten: bool, reason: &str) -> Step {
        Strategy::on_kill(self, now_ms, flatten, reason)
    }
    fn resume(&mut self, now_ms: u64) -> Step {
        Strategy::resume(self, now_ms)
    }
    fn suspend(&mut self) {
        Strategy::suspend(self)
    }
    fn adopt_position(
        &mut self,
        side: Side,
        size: f64,
        entry_px: f64,
        opened_ms: u64,
    ) -> Result<(), String> {
        Strategy::adopt_position(self, side, size, entry_px, opened_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Drive the premium strategy only through the trait (what the engine
    /// and runner see).
    fn generic_flat_check<S: TradingStrategy>(s: &mut S) {
        assert!(matches!(s.phase(), Phase::Flat));
        assert!(s.position().is_none() && s.working().is_none());
        s.adopt_position(Side::Long, 2.0, 100.0, 1_000).unwrap();
        assert_eq!(s.position().unwrap().size, 2.0);
        let st = s.on_kill(2_000, false, "test kill");
        assert!(!st.events.is_empty());
        assert!(matches!(s.phase(), Phase::Halted { .. }));
        let saved = serde_json::to_value(s.state()).unwrap();
        let back: S::State = serde_json::from_value(saved).unwrap();
        assert_eq!(&back, s.state());
    }

    #[test]
    fn premium_strategy_implements_the_trait() {
        let mut s = Strategy::new(StrategyParams::from_config(&Config::default(), 2, 10));
        assert_eq!(TradingStrategy::name(&s), "premium_mean_reversion");
        generic_flat_check(&mut s);
    }
}
