//! The trading engine: market data -> strategy -> risk -> commands.
//!
//! Synchronous and I/O-free, shared by the live / paper main loop
//! ([`crate::runner`]) and the `replay` subcommand. The caller executes the
//! returned [`Command`]s with an executor and feeds the executor's
//! [`ExecEvent`]s back through [`Engine::on_exec`], which may produce more
//! commands (loop until empty).
//!
//! Per market update ([`Engine::on_market`]):
//! 1. build the [`MarketView`] (configured premium source, staleness);
//! 2. rolling mean of the premium (`premium_avg_secs`, the backtest's hourly
//!    signal by default); no premium until the window is warm;
//! 3. risk: persistent-premium rule (fresh data only), equity (daily loss,
//!    drawdown); a kill order is forwarded to the strategy (`on_kill`);
//! 4. strategy tick with `entries_allowed` from the risk manager.
//!
//! Closed trades go to the ledger and to the hit-rate rule. Everything the
//! operator should see is queued as [`Notice`]s (drained by the caller for
//! logs, CSV and alerts).
//!
//! The engine is generic over the strategy ([`TradingStrategy`]); the
//! default type parameter is the premium mean-reversion [`Strategy`], so
//! `Engine` alone keeps meaning what it always did.

use crate::config::{Config, MarketConfig, PremiumSource};
use crate::exec::state::Ledger;
use crate::exec::ExecEvent;
use crate::hl::assets::AssetSpec;
use crate::hl::market::MarketState;
use crate::risk::{KillOrder, RiskManager};
use crate::strategy::premium::PremiumAverager;
use crate::strategy::signal::{
    Command, Event, MarketView, Phase, Step, Strategy, TickInput, TradeRecord,
};
use crate::strategy::TradingStrategy;

/// Something to log / alert, with its time.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    Strategy(u64, Event),
    Kill(u64, KillOrder),
    SpecChange {
        at_ms: u64,
        changes: Vec<String>,
        killed: bool,
    },
    Exec(u64, String),
    Funding(u64, f64),
}

#[derive(Debug, Clone)]
pub struct EngineParams {
    pub premium_source: PremiumSource,
    pub stale_ms: u64,
    pub market: MarketConfig,
    /// Minimum gap between two equity-curve points.
    pub equity_sample_ms: u64,
}

impl EngineParams {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            premium_source: cfg.strategy.premium_source,
            stale_ms: cfg.timing.ws_stale_secs * 1000,
            market: cfg.market.clone(),
            equity_sample_ms: 60_000,
        }
    }
}

pub struct Engine<S: TradingStrategy = Strategy> {
    p: EngineParams,
    pub strategy: S,
    pub risk: RiskManager,
    pub ledger: Ledger,
    averager: PremiumAverager,
    last_mark: Option<f64>,
    last_view: Option<MarketView>,
    /// Instantaneous (not averaged) premium of the last fresh view.
    last_raw_premium_bps: Option<f64>,
    notices: Vec<Notice>,
}

impl<S: TradingStrategy> Engine<S> {
    pub fn new(
        p: EngineParams,
        strategy: S,
        risk: RiskManager,
        ledger: Ledger,
        premium_avg_ms: u64,
    ) -> Self {
        Self {
            p,
            strategy,
            risk,
            ledger,
            averager: PremiumAverager::new(premium_avg_ms),
            last_mark: None,
            last_view: None,
            last_raw_premium_bps: None,
            notices: Vec::new(),
        }
    }

    pub fn drain_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    pub fn last_view(&self) -> Option<&MarketView> {
        self.last_view.as_ref()
    }

    pub fn last_mark(&self) -> Option<f64> {
        self.last_mark
    }

    /// Instantaneous premium (bps) behind the last view's averaged value.
    pub fn last_raw_premium_bps(&self) -> Option<f64> {
        self.last_raw_premium_bps
    }

    /// The premium rolling window is covered enough to trade on.
    pub fn premium_ready(&self) -> bool {
        self.averager.ready()
    }

    /// Premium samples to persist (empty without averaging).
    pub fn premium_samples(&self) -> Vec<(u64, f64)> {
        self.averager.samples()
    }

    /// Restore persisted premium samples.
    pub fn restore_premium_samples(&mut self, now_ms: u64, samples: &[(u64, f64)]) {
        self.averager.restore(now_ms, samples);
    }

    /// Graceful shutdown: the strategy records feedback but issues nothing.
    pub fn suspend(&mut self) {
        self.strategy.suspend();
    }

    /// Strategy-level equity: realized ledger + open position mark-to-market
    /// (net of its fees and funding so far).
    pub fn equity(&self) -> f64 {
        let open = match (self.strategy.position(), self.last_mark) {
            (Some(p), Some(m)) => p.unrealized_pnl(m),
            _ => 0.0,
        };
        self.ledger.realized_pnl_usd + open
    }

    /// Reconcile the persisted strategy phase with the persisted kill state
    /// at startup: a kill that is still active halts the strategy (keeping a
    /// kill flatten in progress), a kill cleared by `reset-kill` resumes it.
    pub fn startup(&mut self, now_ms: u64) -> Vec<Command> {
        let killed = self.risk.kill_record().cloned();
        match (killed, self.strategy.phase().clone()) {
            (Some(_), Phase::Halted { .. }) => Vec::new(),
            (Some(k), _) => {
                let flatten = self.risk.params().flatten_on_kill;
                let st = self.strategy.on_kill(now_ms, flatten, &k.message);
                self.absorb(now_ms, st)
            }
            (None, Phase::Halted { .. }) => {
                let st = self.strategy.resume(now_ms);
                self.absorb(now_ms, st)
            }
            (None, _) => Vec::new(),
        }
    }

    /// Market update (also called on a timer so time-based rules run).
    pub fn on_market(
        &mut self,
        now_ms: u64,
        m: &MarketState,
        available_margin_usd: Option<f64>,
    ) -> Vec<Command> {
        let mut view = MarketView::from_market(m, now_ms, self.p.premium_source, self.p.stale_ms);
        if let Some(mark) = m.mark_px.filter(|x| x.is_finite() && *x > 0.0) {
            self.last_mark = Some(mark);
        }
        self.on_view(now_ms, &mut view, available_margin_usd)
    }

    /// Core of [`Engine::on_market`] on an already built view.
    pub fn on_view(
        &mut self,
        now_ms: u64,
        view: &mut MarketView,
        available_margin_usd: Option<f64>,
    ) -> Vec<Command> {
        if !view.stale {
            self.last_raw_premium_bps = view.premium_bps;
            if let Some(p) = view.premium_bps {
                let avg = self.averager.push(now_ms, p);
                // Until the window is covered the mean is not the signal.
                view.premium_bps = self.averager.ready().then_some(avg);
            }
        } else if self.averager.window_ms() > 0 {
            // Stale: keep the last mean (never the raw value) for records.
            view.premium_bps = self.averager.mean().filter(|_| self.averager.ready());
        }
        self.last_view = Some(*view);
        let mut cmds = Vec::new();
        if !view.stale {
            if let Some(k) = self.risk.on_premium(now_ms, view.premium_bps) {
                cmds.extend(self.apply_kill(now_ms, k));
            }
        }
        let eq = self.equity();
        if let Some(k) = self.risk.on_equity(now_ms, eq) {
            cmds.extend(self.apply_kill(now_ms, k));
        }
        if self
            .ledger
            .equity_curve
            .last()
            .is_none_or(|(t, _)| now_ms.saturating_sub(*t) >= self.p.equity_sample_ms)
        {
            self.ledger.push_equity(now_ms, eq);
        }
        let st = self.strategy.on_tick(TickInput {
            view: *view,
            entries_allowed: self.risk.entries_allowed(),
            available_margin_usd,
        });
        cmds.extend(self.absorb(now_ms, st));
        cmds
    }

    /// Executor feedback.
    pub fn on_exec(&mut self, now_ms: u64, ev: ExecEvent) -> Vec<Command> {
        match ev {
            ExecEvent::Order(u) => {
                let st = self.strategy.on_order_update(now_ms, u);
                self.absorb(now_ms, st)
            }
            ExecEvent::Funding { usd, time_ms } => {
                self.strategy.on_funding(usd);
                self.notices.push(Notice::Funding(time_ms, usd));
                Vec::new()
            }
            ExecEvent::Alert(msg) => {
                self.notices.push(Notice::Exec(now_ms, msg));
                Vec::new()
            }
        }
    }

    /// Result of a meta poll: the live spec, or why it is unavailable.
    pub fn on_spec(&mut self, now_ms: u64, spec: Result<&AssetSpec, String>) -> Vec<Command> {
        let check = match spec {
            Ok(s) => self.risk.on_spec(now_ms, s, &self.p.market),
            Err(what) => self.risk.on_spec_unavailable(now_ms, &what),
        };
        if check.changes.is_empty() {
            return Vec::new();
        }
        self.notices.push(Notice::SpecChange {
            at_ms: now_ms,
            changes: check.changes.clone(),
            killed: check.kill.is_some(),
        });
        match check.kill {
            Some(k) => self.apply_kill(now_ms, k),
            None => Vec::new(),
        }
    }

    /// Operator / fatal-error kill.
    pub fn manual_kill(&mut self, now_ms: u64, note: &str) -> Vec<Command> {
        match self.risk.trigger_manual(now_ms, note) {
            Some(k) => self.apply_kill(now_ms, k),
            None => Vec::new(),
        }
    }

    /// Manual kill with an explicit flatten choice (e.g. a position
    /// mismatch: never trade on a size the bot cannot trust).
    pub fn manual_kill_with(&mut self, now_ms: u64, note: &str, flatten: bool) -> Vec<Command> {
        match self.risk.trigger_manual(now_ms, note) {
            Some(mut k) => {
                k.flatten = flatten;
                self.apply_kill(now_ms, k)
            }
            None => Vec::new(),
        }
    }

    /// Signed strategy position (> 0 long).
    pub fn strategy_position(&self) -> f64 {
        self.strategy
            .position()
            .map_or(0.0, |p| p.side.sign() * p.size)
    }

    fn apply_kill(&mut self, now_ms: u64, k: KillOrder) -> Vec<Command> {
        let st = self.strategy.on_kill(now_ms, k.flatten, &k.alert);
        self.notices.push(Notice::Kill(now_ms, k));
        self.absorb(now_ms, st)
    }

    /// Handle a strategy step: record events, closed trades -> ledger and
    /// hit-rate rule (which may kill and produce more commands).
    fn absorb(&mut self, now_ms: u64, st: Step) -> Vec<Command> {
        let mut cmds = st.commands;
        for ev in st.events {
            if let Event::TradeClosed(t) = &ev {
                cmds.extend(self.trade_closed(now_ms, t));
            }
            self.notices.push(Notice::Strategy(now_ms, ev));
        }
        cmds
    }

    fn trade_closed(&mut self, now_ms: u64, t: &TradeRecord) -> Vec<Command> {
        self.ledger.record_trade(t);
        let eq = self.equity();
        self.ledger.push_equity(now_ms, eq);
        match self.risk.on_trade_closed(now_ms, t) {
            Some(k) => self.apply_kill(now_ms, k),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::paper::{PaperParams, PaperSim};
    use crate::hl::market::DataSource;
    use crate::hl::types::{L2Book, Level};
    use crate::risk::{RiskParams, RiskState};
    use crate::strategy::signal::{Phase, StrategyParams};

    const T0: u64 = 1_800_000_000_000;

    /// Engine on the instantaneous premium (most tests drive single
    /// snapshots; averaging is covered by `premium_average_warm_up`).
    fn engine(cfg: &Config) -> Engine {
        engine_avg(cfg, 0)
    }

    fn engine_avg(cfg: &Config, premium_avg_ms: u64) -> Engine {
        Engine::new(
            EngineParams::from_config(cfg),
            Strategy::new(StrategyParams::from_config(cfg, 2, 10)),
            RiskManager::new(RiskParams::from(&cfg.risk), RiskState::default()),
            Ledger::default(),
            premium_avg_ms,
        )
    }

    fn market(now: u64, bid: f64, ask: f64, oracle: f64) -> MarketState {
        let mut m = MarketState::new("xyz:ANT");
        let l = |px| Level {
            px,
            sz: 100.0,
            n: 1,
        };
        m.apply_book(
            &L2Book {
                coin: "xyz:ANT".into(),
                time: now,
                levels: (vec![l(bid)], vec![l(ask)]),
            },
            now,
            DataSource::Rest,
        );
        m.oracle_px = Some(oracle);
        m.mark_px = Some(0.5 * (bid + ask));
        m.impact_bid_px = Some(bid);
        m.impact_ask_px = Some(ask);
        m.ctx_recv_ms = Some(now);
        m.funding = Some(0.0);
        m
    }

    /// Execute commands against the paper sim until quiet.
    fn pump(e: &mut Engine, sim: &mut PaperSim, now: u64, m: &MarketState, cmds: Vec<Command>) {
        let mut q = cmds;
        for _ in 0..50 {
            if q.is_empty() {
                return;
            }
            let mut next = Vec::new();
            for c in q {
                for ev in sim.execute(now, c, m) {
                    next.extend(e.on_exec(now, ev));
                }
            }
            q = next;
        }
        panic!("command loop did not settle");
    }

    #[test]
    fn full_paper_round_trip() {
        let cfg = Config::default();
        let mut e = engine(&cfg);
        let mut sim = PaperSim::new(PaperParams::from_config(&cfg, 2));
        // Premium +100 bps (mid 101 vs oracle 100): short entry ALO at the ask.
        let m = market(T0, 100.99, 101.01, 100.0);
        let cmds = e.on_market(T0, &m, None);
        assert_eq!(cmds.len(), 1);
        pump(&mut e, &mut sim, T0, &m, cmds);
        assert_eq!(sim.open_orders().len(), 1);
        assert!(matches!(e.strategy.phase(), Phase::Entering { .. }));
        // A buyer lifts our ask: filled, position open.
        let t = crate::hl::types::WsTrade {
            coin: "xyz:ANT".into(),
            side: "B".into(),
            px: 101.01,
            sz: 1000.0,
            time: T0 + 1_000,
            tid: 9,
        };
        for ev in sim.on_trades(&[t]) {
            let c = e.on_exec(T0 + 1_000, ev);
            pump(&mut e, &mut sim, T0 + 1_000, &m, c);
        }
        assert!(matches!(e.strategy.phase(), Phase::InPosition));
        assert!(sim.account().position_sz < 0.0);
        // Premium converges to 0: maker exit at the bid.
        let m2 = market(T0 + 60_000, 100.00, 100.02, 100.01);
        let c = e.on_market(T0 + 60_000, &m2, None);
        pump(&mut e, &mut sim, T0 + 60_000, &m2, c);
        assert!(matches!(e.strategy.phase(), Phase::Exiting { .. }));
        // Ask drops onto our bid: exit filled, trade closed, profit.
        let m3 = market(T0 + 61_000, 99.98, 100.00, 100.01);
        for ev in sim.on_book(T0 + 61_000, &m3) {
            let c = e.on_exec(T0 + 61_000, ev);
            pump(&mut e, &mut sim, T0 + 61_000, &m3, c);
        }
        assert_eq!(e.ledger.trades.len(), 1);
        let t = &e.ledger.trades[0];
        assert!(t.pnl_usd > 0.0, "{t:?}");
        assert!((e.ledger.realized_pnl_usd - t.pnl_usd).abs() < 1e-12);
        // Strategy ledger and simulated account agree.
        assert!(
            (sim.account().net_pnl(None) - t.pnl_usd).abs() < 1e-9,
            "{} vs {}",
            sim.account().net_pnl(None),
            t.pnl_usd
        );
        assert_eq!(e.risk.state().trades_closed, 1);
        let n = e.drain_notices();
        assert!(n
            .iter()
            .any(|x| matches!(x, Notice::Strategy(_, Event::TradeClosed(_)))));
    }

    #[test]
    fn spec_change_kills_and_halts() {
        let cfg = Config::default();
        let mut e = engine(&cfg);
        let c = e.on_spec(T0, Err("xyz:ANT missing from meta".into()));
        assert!(c.is_empty());
        assert!(e.risk.is_killed());
        assert!(matches!(e.strategy.phase(), Phase::Halted { .. }));
        let n = e.drain_notices();
        assert!(n.iter().any(|x| matches!(x, Notice::Kill(..))));
        assert!(n
            .iter()
            .any(|x| matches!(x, Notice::SpecChange { killed: true, .. })));
        // Entries stay blocked.
        let m = market(T0 + 1, 100.99, 101.01, 100.0);
        assert!(e.on_market(T0 + 1, &m, None).is_empty());
    }

    #[test]
    fn startup_honours_persisted_kill_and_reset() {
        let cfg = Config::default();
        let mut e = engine(&cfg);
        e.manual_kill(T0, "test");
        let halted = e.strategy.state().clone();
        // Restart with the kill still active: stays halted.
        let mut e2 = Engine::new(
            EngineParams::from_config(&cfg),
            Strategy::from_state(StrategyParams::from_config(&cfg, 2, 10), halted.clone()),
            RiskManager::new(RiskParams::from(&cfg.risk), e.risk.state().clone()),
            Ledger::default(),
            0,
        );
        assert!(e2.startup(T0 + 1).is_empty());
        assert!(matches!(e2.strategy.phase(), Phase::Halted { .. }));
        // Kill cleared by reset-kill: the strategy resumes.
        let mut rs = e.risk.state().clone();
        crate::risk::kill::reset_state(&mut rs);
        let mut e3 = Engine::new(
            EngineParams::from_config(&cfg),
            Strategy::from_state(StrategyParams::from_config(&cfg, 2, 10), halted),
            RiskManager::new(RiskParams::from(&cfg.risk), rs),
            Ledger::default(),
            0,
        );
        e3.startup(T0 + 2);
        assert!(matches!(e3.strategy.phase(), Phase::Flat));
        // Killed in risk but strategy not halted (crash in between): halt now.
        let mut e4 = Engine::new(
            EngineParams::from_config(&cfg),
            Strategy::new(StrategyParams::from_config(&cfg, 2, 10)),
            RiskManager::new(RiskParams::from(&cfg.risk), e.risk.state().clone()),
            Ledger::default(),
            0,
        );
        e4.startup(T0 + 3);
        assert!(matches!(e4.strategy.phase(), Phase::Halted { .. }));
    }

    #[test]
    fn stale_data_does_not_feed_the_premium_rule() {
        let mut cfg = Config::default();
        cfg.risk.persistent_premium_secs = 1;
        let mut e = engine(&cfg);
        let mut m = market(T0, 101.99, 102.01, 100.0); // +200 bps
        m.ctx_recv_ms = Some(T0 - 60_000); // stale ctx
        e.on_market(T0, &m, None);
        e.on_market(T0 + 5_000, &m, None);
        assert!(!e.risk.is_killed());
        let m = market(T0 + 10_000, 101.99, 102.01, 100.0);
        e.on_market(T0 + 10_000, &m, None);
        e.on_market(
            T0 + 12_000,
            &market(T0 + 12_000, 101.99, 102.01, 100.0),
            None,
        );
        assert!(e.risk.is_killed());
    }

    #[test]
    fn premium_average_warm_up() {
        // Default config: hourly rolling mean, the backtested signal.
        let cfg = Config::default();
        assert_eq!(cfg.strategy.premium_avg_secs, 3600);
        let mut e = engine_avg(&cfg, cfg.strategy.premium_avg_secs * 1000);
        // +100 bps for 10 minutes: window not covered, no signal, no order.
        let mut now = T0;
        for _ in 0..120 {
            let cmds = e.on_market(now, &market(now, 100.99, 101.01, 100.0), None);
            assert!(cmds.is_empty(), "entered before the window was warm");
            assert!(e.last_view().unwrap().premium_bps.is_none());
            assert!(e.last_raw_premium_bps().is_some());
            now += 5_000;
        }
        assert!(!e.premium_ready());
        // Restore a covered hour of samples at +100 bps: ready, entry.
        let samples: Vec<(u64, f64)> = (0..720)
            .map(|i| (now - 3_600_000 + i * 5_000, 100.0))
            .collect();
        let mut e2 = engine_avg(&cfg, cfg.strategy.premium_avg_secs * 1000);
        e2.restore_premium_samples(now, &samples);
        assert!(e2.premium_ready());
        let cmds = e2.on_market(now, &market(now, 100.99, 101.01, 100.0), None);
        assert_eq!(cmds.len(), 1);
        let p = e2.last_view().unwrap().premium_bps.unwrap();
        assert!((p - 100.0).abs() < 1.0, "{p}");
        // A single sample at 0 barely moves the hourly mean (no exit flicker).
        e2.on_market(
            now + 5_000,
            &market(now + 5_000, 99.99, 100.01, 100.0),
            None,
        );
        assert!(e2.last_view().unwrap().premium_bps.unwrap() > 90.0);
        assert!(e2.last_raw_premium_bps().unwrap().abs() < 1.0);
    }
}
