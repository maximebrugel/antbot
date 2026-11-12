//! Signal state machine (pure, deterministic, no I/O, no clock).
//!
//! ```text
//!            |premium| >= entry                 fully filled / timeout with fill
//!   Flat ─────────────────────► Entering ─────────────────────────────► InPosition
//!    ▲  (cooldown, entries ok)   │ ALO at touch, requote on drift          │
//!    │                           │ timeout / signal gone / blocked,        │ converged (early exit)
//!    │◄──────────────────────────┘ nothing filled (retry cooldown)         │ or deadline - window - maker_timeout
//!    │                                                                     ▼
//!    └──────────────────────── position closed (cooldown) ◄──────────── Exiting
//!                                                               maker ALO (reduce-only) first,
//!                                                               IOC taker from `taker_at`
//!   any phase ── kill ──► Halted (cancel orders; flatten first when configured)
//! ```
//!
//! Inputs are market ticks ([`TickInput`]), order feedback from the executor
//! ([`OrderUpdate`]), funding payments and kill / resume calls. Outputs are
//! [`Command`]s (place / cancel) for the executor plus [`Event`]s for
//! logging, CSV and alerts. Time is always supplied by the caller (ms since
//! the epoch), so the same inputs always produce the same outputs.
//!
//! # Executor contract
//!
//! * Every `Place` is eventually answered by exactly one terminal
//!   [`OrderUpdate::Done`] (filled, canceled, rejected, IOC remainder
//!   expired). Fills come as [`OrderUpdate::Filled`], possibly partial, and
//!   may arrive before or after `Done`.
//! * `tag` is the strategy's order id; the executor maps it to a cloid.
//! * Prices and sizes in commands are already on the tick / lot grid
//!   (`hl::rounding`). Exit orders are reduce-only.
//!
//! # Rules
//!
//! * One position at a time and at most one working order at any time.
//!   Requotes are cancel → wait for `Done` → place, so there is never more
//!   than one live order and no risk of a double fill.
//! * Entry: maker-only ALO at the touch (buy at best bid, sell at best ask).
//!   Requote once the order has rested `requote_interval_secs` and the touch
//!   has moved `requote_drift_bps` away. Abandon after `entry_timeout_secs`,
//!   when the premium has already converged (`converged()`), when entries
//!   get blocked by risk, or when the exchange rejects the order (other than
//!   a post-only reject, which is simply requoted). A partial fill becomes
//!   the position.
//! * Hold clock: starts at the first entry fill. Deadline = open +
//!   `max_hold_secs` (8h).
//! * Exit triggers: convergence (`early_exit_on_converge`, side-adjusted
//!   premium <= `exit_converge_bps`), or the deadline schedule: the maker exit
//!   starts at `deadline - taker_fallback_window - exit_maker_timeout`, and
//!   IOC takes over at `deadline - taker_fallback_window`. A convergence exit
//!   goes taker after `exit_maker_timeout_secs`, a kill flatten after
//!   `kill_flatten_maker_secs`, and never later than
//!   `deadline - taker_fallback_window`. The window is the buffer that lets
//!   the IOC (and its retries) finish before the 8h deadline.
//! * After a closed trade: `cooldown_after_exit_secs` before the next entry
//!   (backtest: next signal from exit hour + 1h).
//! * Order pacing: at least `min_order_interval_ms` between new orders and at
//!   most `max_order_actions_per_min` actions (orders + cancels) per rolling
//!   minute. Cancels are counted but never delayed.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::premium::{converged, entry_signal, from_bps, to_bps, Side};
use super::sizing::{lot_epsilon, plan_entry};
use crate::config::{Config, PremiumSource, SizingConfig};
use crate::hl::market::MarketState;
use crate::hl::rounding::{round_price, round_size, RoundDir, MIN_ORDER_NOTIONAL_USD};

const MINUTE_MS: u64 = 60_000;
const RECENT_DONE_KEEP: usize = 16;

/// Strategy parameters, all durations in ms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyParams {
    pub entry_threshold_bps: f64,
    pub exit_converge_bps: f64,
    pub early_exit_on_converge: bool,
    pub max_hold_ms: u64,
    pub cooldown_ms: u64,
    pub entry_timeout_ms: u64,
    pub entry_retry_cooldown_ms: u64,
    pub requote_interval_ms: u64,
    pub requote_drift_bps: f64,
    pub taker_fallback_window_ms: u64,
    pub exit_maker_timeout_ms: u64,
    pub kill_flatten_maker_ms: u64,
    pub taker_max_slippage_bps: f64,
    pub min_order_interval_ms: u64,
    pub max_order_actions_per_min: u32,
    pub sizing: SizingConfig,
    pub sz_decimals: u32,
    pub asset_max_leverage: u32,
}

impl StrategyParams {
    /// Build from the config and the resolved asset spec.
    pub fn from_config(cfg: &Config, sz_decimals: u32, asset_max_leverage: u32) -> Self {
        let s = &cfg.strategy;
        Self {
            entry_threshold_bps: s.entry_threshold_bps,
            exit_converge_bps: s.exit_converge_bps,
            early_exit_on_converge: s.early_exit_on_converge,
            max_hold_ms: s.max_hold_secs * 1000,
            cooldown_ms: s.cooldown_after_exit_secs * 1000,
            entry_timeout_ms: s.entry_timeout_secs * 1000,
            entry_retry_cooldown_ms: s.entry_retry_cooldown_secs * 1000,
            requote_interval_ms: s.requote_interval_secs * 1000,
            requote_drift_bps: s.requote_drift_bps,
            taker_fallback_window_ms: s.taker_fallback_window_secs * 1000,
            exit_maker_timeout_ms: s.exit_maker_timeout_secs * 1000,
            kill_flatten_maker_ms: cfg.risk.kill_flatten_maker_secs * 1000,
            taker_max_slippage_bps: s.taker_max_slippage_bps,
            min_order_interval_ms: s.min_order_interval_ms,
            max_order_actions_per_min: s.max_order_actions_per_min,
            sizing: cfg.sizing.clone(),
            sz_decimals,
            asset_max_leverage,
        }
    }
}

/// What the strategy needs from the market at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct MarketView {
    pub now_ms: u64,
    /// Premium in bps (configured source, optionally averaged).
    pub premium_bps: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    /// Data too old to act on: no new orders, no requotes, no convergence exit.
    pub stale: bool,
}

impl MarketView {
    /// Build from the feed's [`MarketState`] (instantaneous premium; apply a
    /// [`super::premium::PremiumAverager`] on `premium_bps` if configured).
    pub fn from_market(
        m: &MarketState,
        now_ms: u64,
        source: PremiumSource,
        max_age_ms: u64,
    ) -> Self {
        Self {
            now_ms,
            premium_bps: m.premium(source).map(to_bps),
            best_bid: m.best_bid.map(|l| l.px),
            best_ask: m.best_ask.map(|l| l.px),
            stale: m.is_stale(now_ms, max_age_ms, source),
        }
    }

    fn usable(&self) -> bool {
        !self.stale
    }
}

/// One tick of input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TickInput {
    pub view: MarketView,
    /// False when the risk manager blocks new entries (kill, etc.).
    pub entries_allowed: bool,
    /// Margin available for a new position, if known (live mode).
    pub available_margin_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderKind {
    /// Post-only limit (maker).
    Alo,
    /// Immediate-or-cancel limit (taker).
    Ioc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    Entry,
    Exit,
}

/// An order the executor must place.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OrderIntent {
    pub tag: u64,
    pub is_buy: bool,
    pub px: f64,
    pub sz: f64,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub purpose: Purpose,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Place(OrderIntent),
    Cancel { tag: u64 },
}

/// Terminal state of an order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DoneReason {
    Filled,
    Canceled,
    /// ALO would have crossed the book.
    PostOnlyRejected,
    /// Any other rejection (margin, tick, halted market...).
    Rejected(String),
    /// IOC remainder that did not match.
    IocExpired,
}

/// Order feedback from the executor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OrderUpdate {
    /// Order acknowledged and resting.
    Resting { tag: u64 },
    /// (Partial) fill. `tag` is `None` for fills the executor cannot map.
    Filled {
        tag: Option<u64>,
        is_buy: bool,
        px: f64,
        sz: f64,
        fee_usd: f64,
        /// True if we were the taker.
        crossed: bool,
        time_ms: u64,
    },
    /// Terminal state.
    Done { tag: u64, reason: DoneReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    Timeout,
    SignalGone,
    EntriesBlocked,
    Rejected,
    Killed { flatten: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Premium converged to within the exit threshold.
    Converged,
    /// Max hold deadline schedule.
    MaxHold,
    /// Kill switch flatten.
    Kill,
    /// Position closed by fills outside an exit (manual / external).
    External,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Converged => "converged",
            ExitReason::MaxHold => "max_hold",
            ExitReason::Kill => "kill",
            ExitReason::External => "external",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Flat,
    Entering {
        side: Side,
        started_ms: u64,
        timeout_at_ms: u64,
        target_sz: f64,
        signal_premium_bps: f64,
        abort: Option<AbortReason>,
    },
    InPosition,
    Exiting {
        reason: ExitReason,
        started_ms: u64,
        taker_at_ms: u64,
        premium_at_start_bps: Option<f64>,
        taker_announced: bool,
    },
    Halted {
        reason: String,
    },
}

/// Discriminant of [`Phase`] for comparisons and display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    Flat,
    Entering,
    InPosition,
    Exiting,
    Halted,
}

impl PhaseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PhaseKind::Flat => "flat",
            PhaseKind::Entering => "entering",
            PhaseKind::InPosition => "in_position",
            PhaseKind::Exiting => "exiting",
            PhaseKind::Halted => "halted",
        }
    }
}

impl Phase {
    pub fn kind(&self) -> PhaseKind {
        match self {
            Phase::Flat => PhaseKind::Flat,
            Phase::Entering { .. } => PhaseKind::Entering,
            Phase::InPosition => PhaseKind::InPosition,
            Phase::Exiting { .. } => PhaseKind::Exiting,
            Phase::Halted { .. } => PhaseKind::Halted,
        }
    }
}

/// The open trade's accounting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionBook {
    pub side: Side,
    /// Current open size (>= 0).
    pub size: f64,
    pub entry_sz: f64,
    pub entry_notional: f64,
    pub exit_sz: f64,
    pub exit_notional: f64,
    pub exit_taker_sz: f64,
    pub fees_usd: f64,
    /// Funding received (> 0) or paid (< 0).
    pub funding_usd: f64,
    pub signal_ms: u64,
    /// Time of the first entry fill (hold clock start).
    pub opened_ms: u64,
    pub entry_premium_bps: Option<f64>,
}

impl PositionBook {
    pub fn avg_entry_px(&self) -> Option<f64> {
        (self.entry_sz > 0.0).then(|| self.entry_notional / self.entry_sz)
    }

    pub fn avg_exit_px(&self) -> Option<f64> {
        (self.exit_sz > 0.0).then(|| self.exit_notional / self.exit_sz)
    }

    /// Mark-to-market PnL (USD, net of fees and funding so far) at `mark_px`.
    pub fn unrealized_pnl(&self, mark_px: f64) -> f64 {
        let entry = self.avg_entry_px().unwrap_or(mark_px);
        let open = self.side.sign() * (mark_px - entry) * self.size;
        let closed = match self.avg_exit_px() {
            Some(x) => self.side.sign() * (x - entry) * self.exit_sz,
            None => 0.0,
        };
        open + closed - self.fees_usd + self.funding_usd
    }

    pub fn deadline_ms(&self, max_hold_ms: u64) -> u64 {
        self.opened_ms.saturating_add(max_hold_ms)
    }
}

/// A closed round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeRecord {
    pub side: Side,
    pub size: f64,
    pub entry_px: f64,
    pub exit_px: f64,
    pub signal_ms: u64,
    pub opened_ms: u64,
    pub closed_ms: u64,
    pub entry_premium_bps: Option<f64>,
    pub exit_premium_bps: Option<f64>,
    pub fees_usd: f64,
    pub funding_usd: f64,
    /// Price PnL before fees and funding.
    pub gross_pnl_usd: f64,
    /// `gross - fees + funding`.
    pub pnl_usd: f64,
    /// Net PnL in bps of the entry notional.
    pub pnl_bps: f64,
    pub exit_reason: ExitReason,
    /// Part of the exit done as taker.
    pub exit_taker_sz: f64,
}

impl TradeRecord {
    pub fn hold_secs(&self) -> f64 {
        self.closed_ms.saturating_sub(self.opened_ms) as f64 / 1000.0
    }

    pub fn entry_notional_usd(&self) -> f64 {
        self.entry_px * self.size
    }

    /// Writeup hit: `|p_exit| < |p_entry|`; false if either is unknown.
    pub fn moved_toward_zero(&self) -> bool {
        match (self.entry_premium_bps, self.exit_premium_bps) {
            (Some(e), Some(x)) => super::premium::moved_toward_zero(e, x),
            _ => false,
        }
    }
}

/// Notifications for logs, CSV and alerts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    EntryStarted {
        side: Side,
        premium_bps: f64,
        px: f64,
        size: f64,
        notional_usd: f64,
    },
    EntryAborted {
        reason: AbortReason,
    },
    PositionOpened {
        side: Side,
        size: f64,
        avg_px: f64,
        opened_ms: u64,
    },
    Requote {
        tag: u64,
        old_px: f64,
        new_px: f64,
    },
    ExitStarted {
        reason: ExitReason,
        premium_bps: Option<f64>,
        taker_at_ms: u64,
    },
    TakerFallback {
        remaining: f64,
    },
    OrderRejected {
        tag: u64,
        reason: DoneReason,
    },
    TradeClosed(TradeRecord),
    Halted {
        reason: String,
        flatten: bool,
    },
    Resumed,
    Warning(String),
}

/// Output of one transition.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Step {
    pub commands: Vec<Command>,
    pub events: Vec<Event>,
}

impl Step {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.events.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkingOrder {
    pub intent: OrderIntent,
    pub placed_ms: u64,
    pub filled: f64,
    pub acked: bool,
    pub cancel_requested: bool,
}

/// Complete persisted strategy state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyState {
    pub phase: Phase,
    pub cooldown_until_ms: u64,
    pub position: Option<PositionBook>,
    pub working: Option<WorkingOrder>,
    /// Recently finished orders, to attribute late fills: (tag, is_buy, purpose).
    pub recent_done: VecDeque<(u64, bool, Purpose)>,
    pub next_tag: u64,
    pub last_place_ms: Option<u64>,
    pub action_times: VecDeque<u64>,
    pub last_view: MarketView,
    pub entries_allowed: bool,
    pub available_margin_usd: Option<f64>,
    /// Set by a kill; the phase ends in `Halted` with this reason.
    pub kill_reason: Option<String>,
}

impl Default for StrategyState {
    fn default() -> Self {
        Self {
            phase: Phase::Flat,
            cooldown_until_ms: 0,
            position: None,
            working: None,
            recent_done: VecDeque::new(),
            next_tag: 1,
            last_place_ms: None,
            action_times: VecDeque::new(),
            last_view: MarketView {
                stale: true,
                ..Default::default()
            },
            entries_allowed: true,
            available_margin_usd: None,
            kill_reason: None,
        }
    }
}

/// The strategy state machine.
#[derive(Debug, Clone)]
pub struct Strategy {
    p: StrategyParams,
    s: StrategyState,
}

impl Strategy {
    pub fn new(params: StrategyParams) -> Self {
        Self {
            p: params,
            s: StrategyState::default(),
        }
    }

    /// Resume from a persisted state.
    pub fn from_state(params: StrategyParams, state: StrategyState) -> Self {
        Self {
            p: params,
            s: state,
        }
    }

    pub fn params(&self) -> &StrategyParams {
        &self.p
    }

    pub fn state(&self) -> &StrategyState {
        &self.s
    }

    pub fn phase(&self) -> &Phase {
        &self.s.phase
    }

    pub fn position(&self) -> Option<&PositionBook> {
        self.s.position.as_ref()
    }

    pub fn working(&self) -> Option<&WorkingOrder> {
        self.s.working.as_ref()
    }

    fn eps(&self) -> f64 {
        lot_epsilon(self.p.sz_decimals)
    }

    fn pos_size(&self) -> f64 {
        self.s.position.as_ref().map_or(0.0, |p| p.size)
    }

    // ------------------------------------------------------------ inputs

    /// Market tick.
    pub fn on_tick(&mut self, input: TickInput) -> Step {
        self.s.last_view = input.view;
        self.s.entries_allowed = input.entries_allowed;
        self.s.available_margin_usd = input.available_margin_usd;
        let mut st = Step::default();
        self.advance(input.view.now_ms, &mut st);
        st
    }

    /// Order feedback from the executor.
    pub fn on_order_update(&mut self, now_ms: u64, upd: OrderUpdate) -> Step {
        let mut st = Step::default();
        match upd {
            OrderUpdate::Resting { tag } => {
                if let Some(w) = self.s.working.as_mut().filter(|w| w.intent.tag == tag) {
                    w.acked = true;
                }
                return st;
            }
            OrderUpdate::Filled {
                tag,
                is_buy,
                px,
                sz,
                fee_usd,
                crossed,
                time_ms,
            } => {
                let known = tag.is_some_and(|t| {
                    self.s.working.as_ref().is_some_and(|w| w.intent.tag == t)
                        || self.s.recent_done.iter().any(|(d, _, _)| *d == t)
                });
                if !known {
                    st.events.push(Event::Warning(format!(
                        "fill for unknown order {tag:?}: {} {sz} @ {px}",
                        if is_buy { "buy" } else { "sell" }
                    )));
                }
                if let Some(w) = self
                    .s
                    .working
                    .as_mut()
                    .filter(|w| Some(w.intent.tag) == tag)
                {
                    w.filled += sz;
                }
                self.apply_fill(is_buy, px, sz, fee_usd, crossed, time_ms, &mut st);
            }
            OrderUpdate::Done { tag, reason } => {
                let Some(w) = self.s.working.take_if(|w| w.intent.tag == tag) else {
                    return st; // duplicate / stale Done
                };
                self.s
                    .recent_done
                    .push_back((tag, w.intent.is_buy, w.intent.purpose));
                while self.s.recent_done.len() > RECENT_DONE_KEEP {
                    self.s.recent_done.pop_front();
                }
                match &reason {
                    DoneReason::PostOnlyRejected => st.events.push(Event::OrderRejected {
                        tag,
                        reason: reason.clone(),
                    }),
                    DoneReason::Rejected(_) => {
                        st.events.push(Event::OrderRejected {
                            tag,
                            reason: reason.clone(),
                        });
                        if w.intent.purpose == Purpose::Entry {
                            if let Phase::Entering { abort, .. } = &mut self.s.phase {
                                abort.get_or_insert(AbortReason::Rejected);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.advance(now_ms, &mut st);
        st
    }

    /// Funding paid on the open position (> 0 received).
    pub fn on_funding(&mut self, usd: f64) {
        if let Some(p) = self.s.position.as_mut() {
            if usd.is_finite() {
                p.funding_usd += usd;
            }
        }
    }

    /// Kill switch: stop entries, cancel orders, and flatten if requested;
    /// the machine ends in `Halted` until [`Strategy::resume`].
    pub fn on_kill(&mut self, now_ms: u64, flatten: bool, reason: &str) -> Step {
        let mut st = Step::default();
        if matches!(self.s.phase, Phase::Halted { .. }) {
            return st;
        }
        self.s.kill_reason = Some(reason.to_string());
        st.events.push(Event::Halted {
            reason: reason.to_string(),
            flatten,
        });
        let has_pos = self.pos_size() > self.eps();
        match &mut self.s.phase {
            Phase::Flat => self.halt(),
            Phase::Entering { abort, .. } => *abort = Some(AbortReason::Killed { flatten }),
            Phase::InPosition => {
                if flatten && has_pos {
                    self.start_exit(now_ms, ExitReason::Kill, &mut st);
                } else {
                    self.halt();
                }
            }
            Phase::Exiting {
                reason: r,
                taker_at_ms,
                ..
            } => {
                if flatten {
                    *r = ExitReason::Kill;
                    *taker_at_ms =
                        (*taker_at_ms).min(now_ms.saturating_add(self.p.kill_flatten_maker_ms));
                } else {
                    self.halt();
                }
            }
            Phase::Halted { .. } => {}
        }
        self.advance(now_ms, &mut st);
        st
    }

    /// Manual resume after a kill reset.
    pub fn resume(&mut self, now_ms: u64) -> Step {
        let mut st = Step::default();
        self.s.kill_reason = None;
        // The last view may be old: act only on the next fresh tick.
        self.s.last_view.stale = true;
        if let Phase::Halted { .. } = self.s.phase {
            self.s.phase = if self.pos_size() > self.eps() {
                Phase::InPosition
            } else {
                Phase::Flat
            };
            st.events.push(Event::Resumed);
            self.advance(now_ms, &mut st);
        }
        st
    }

    /// Adopt a position found at startup reconciliation (only when flat).
    pub fn adopt_position(
        &mut self,
        side: Side,
        size: f64,
        entry_px: f64,
        opened_ms: u64,
    ) -> Result<(), String> {
        if self.s.position.is_some() || !matches!(self.s.phase, Phase::Flat | Phase::Halted { .. })
        {
            return Err("strategy already tracks a position or an entry".into());
        }
        if !(size.is_finite() && size > self.eps() && entry_px.is_finite() && entry_px > 0.0) {
            return Err(format!("bad position {size} @ {entry_px}"));
        }
        self.s.position = Some(PositionBook {
            side,
            size,
            entry_sz: size,
            entry_notional: size * entry_px,
            exit_sz: 0.0,
            exit_notional: 0.0,
            exit_taker_sz: 0.0,
            fees_usd: 0.0,
            funding_usd: 0.0,
            signal_ms: opened_ms,
            opened_ms,
            entry_premium_bps: None,
        });
        if matches!(self.s.phase, Phase::Flat) {
            self.s.phase = Phase::InPosition;
        }
        Ok(())
    }

    // ------------------------------------------------------------ internals

    fn halt(&mut self) {
        let reason = self
            .s
            .kill_reason
            .clone()
            .unwrap_or_else(|| "halted".into());
        self.s.phase = Phase::Halted { reason };
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_fill(
        &mut self,
        is_buy: bool,
        px: f64,
        sz: f64,
        fee: f64,
        crossed: bool,
        time_ms: u64,
        st: &mut Step,
    ) {
        if !(px.is_finite() && px > 0.0 && sz.is_finite() && sz > 0.0) {
            st.events
                .push(Event::Warning(format!("ignored invalid fill {sz} @ {px}")));
            return;
        }
        let fee = if fee.is_finite() { fee } else { 0.0 };
        let eps = self.eps();
        let (signal_ms, entry_premium) = match &self.s.phase {
            Phase::Entering {
                started_ms,
                signal_premium_bps,
                ..
            } => (*started_ms, Some(*signal_premium_bps)),
            _ => (time_ms, self.s.last_view.premium_bps),
        };
        let pos = self.s.position.get_or_insert(PositionBook {
            side: if is_buy { Side::Long } else { Side::Short },
            size: 0.0,
            entry_sz: 0.0,
            entry_notional: 0.0,
            exit_sz: 0.0,
            exit_notional: 0.0,
            exit_taker_sz: 0.0,
            fees_usd: 0.0,
            funding_usd: 0.0,
            signal_ms,
            opened_ms: time_ms,
            entry_premium_bps: entry_premium,
        });
        pos.fees_usd += fee;
        if is_buy == pos.side.open_is_buy() {
            if pos.entry_sz <= 0.0 {
                pos.opened_ms = time_ms;
            }
            pos.size += sz;
            pos.entry_sz += sz;
            pos.entry_notional += px * sz;
        } else {
            let q = sz.min(pos.size);
            if sz > pos.size + eps {
                st.events.push(Event::Warning(format!(
                    "closing fill {sz} exceeds open size {}; excess ignored",
                    pos.size
                )));
            }
            pos.size -= q;
            if pos.size < eps {
                pos.size = 0.0;
            }
            pos.exit_sz += q;
            pos.exit_notional += px * q;
            if crossed {
                pos.exit_taker_sz += q;
            }
        }
    }

    fn prune_actions(&mut self, now: u64) {
        while self
            .s
            .action_times
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= MINUTE_MS)
        {
            self.s.action_times.pop_front();
        }
    }

    fn can_place(&mut self, now: u64) -> bool {
        self.prune_actions(now);
        let gap_ok = self
            .s
            .last_place_ms
            .is_none_or(|t| now.saturating_sub(t) >= self.p.min_order_interval_ms);
        gap_ok && (self.s.action_times.len() as u32) < self.p.max_order_actions_per_min
    }

    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        now: u64,
        is_buy: bool,
        px: f64,
        sz: f64,
        kind: OrderKind,
        purpose: Purpose,
        st: &mut Step,
    ) {
        let intent = OrderIntent {
            tag: self.s.next_tag,
            is_buy,
            px,
            sz,
            kind,
            reduce_only: purpose == Purpose::Exit,
            purpose,
        };
        self.s.next_tag += 1;
        self.s.last_place_ms = Some(now);
        self.s.action_times.push_back(now);
        self.s.working = Some(WorkingOrder {
            intent,
            placed_ms: now,
            filled: 0.0,
            acked: false,
            cancel_requested: false,
        });
        st.commands.push(Command::Place(intent));
    }

    /// Request cancellation of the working order. IOC orders and fully
    /// filled orders end on their own (their `Done` is pending), so they are
    /// not canceled.
    fn cancel_working(&mut self, now: u64, st: &mut Step) {
        let eps = self.eps();
        if let Some(w) = self.s.working.as_mut() {
            let finished = w.intent.kind == OrderKind::Ioc || w.filled + eps >= w.intent.sz;
            if !w.cancel_requested && !finished {
                w.cancel_requested = true;
                let tag = w.intent.tag;
                self.s.action_times.push_back(now);
                st.commands.push(Command::Cancel { tag });
            }
        }
    }

    /// Passive (maker) price at the touch for this side, on the tick grid.
    fn maker_px(&self, is_buy: bool) -> Option<f64> {
        let v = &self.s.last_view;
        let raw = if is_buy { v.best_bid? } else { v.best_ask? };
        round_price(raw, self.p.sz_decimals, RoundDir::passive(is_buy)).ok()
    }

    /// IOC limit: opposite touch +/- the slippage cap, rounded inside the cap.
    fn taker_px(&self, is_buy: bool) -> Option<f64> {
        let v = &self.s.last_view;
        let slip = from_bps(self.p.taker_max_slippage_bps);
        let raw = if is_buy {
            v.best_ask? * (1.0 + slip)
        } else {
            v.best_bid? * (1.0 - slip)
        };
        round_price(raw, self.p.sz_decimals, RoundDir::passive(is_buy)).ok()
    }

    /// Cancel the working order if the touch moved enough since it was placed.
    fn maybe_requote(&mut self, now: u64, st: &mut Step) {
        let Some(w) = self.s.working.as_ref() else {
            return;
        };
        if w.cancel_requested || w.intent.kind != OrderKind::Alo || !self.s.last_view.usable() {
            return;
        }
        if now.saturating_sub(w.placed_ms) < self.p.requote_interval_ms {
            return;
        }
        let Some(target) = self.maker_px(w.intent.is_buy) else {
            return;
        };
        let drift_bps = to_bps((target - w.intent.px).abs() / w.intent.px);
        if drift_bps > 0.0 && drift_bps + 1e-9 >= self.p.requote_drift_bps {
            st.events.push(Event::Requote {
                tag: w.intent.tag,
                old_px: w.intent.px,
                new_px: target,
            });
            self.cancel_working(now, st);
        }
    }

    fn start_exit(&mut self, now: u64, reason: ExitReason, st: &mut Step) {
        let opened = self.s.position.as_ref().map_or(now, |p| p.opened_ms);
        let deadline = opened.saturating_add(self.p.max_hold_ms);
        let hard_taker = deadline.saturating_sub(self.p.taker_fallback_window_ms);
        let taker_at = match reason {
            ExitReason::MaxHold | ExitReason::External => hard_taker,
            ExitReason::Converged => now
                .saturating_add(self.p.exit_maker_timeout_ms)
                .min(hard_taker),
            ExitReason::Kill => now
                .saturating_add(self.p.kill_flatten_maker_ms)
                .min(hard_taker),
        };
        let premium = self.s.last_view.premium_bps;
        self.s.phase = Phase::Exiting {
            reason,
            started_ms: now,
            taker_at_ms: taker_at,
            premium_at_start_bps: premium,
            taker_announced: false,
        };
        st.events.push(Event::ExitStarted {
            reason,
            premium_bps: premium,
            taker_at_ms: taker_at,
        });
    }

    /// Close the trade record once the position is back to zero.
    fn maybe_close_trade(&mut self, now: u64, st: &mut Step) {
        let eps = self.eps();
        let done = self
            .s
            .position
            .as_ref()
            .is_some_and(|p| p.size < eps && p.entry_sz > eps && p.exit_sz > eps);
        if !done {
            return;
        }
        let p = self.s.position.take().expect("checked");
        let entry_px = p.avg_entry_px().unwrap_or(0.0);
        let exit_px = p.avg_exit_px().unwrap_or(0.0);
        let gross = p.side.sign() * (exit_px - entry_px) * p.exit_sz;
        let net = gross - p.fees_usd + p.funding_usd;
        let notional = entry_px * p.entry_sz;
        let exit_reason = match &self.s.phase {
            Phase::Exiting { reason, .. } => *reason,
            _ => ExitReason::External,
        };
        let rec = TradeRecord {
            side: p.side,
            size: p.entry_sz,
            entry_px,
            exit_px,
            signal_ms: p.signal_ms,
            opened_ms: p.opened_ms,
            closed_ms: now,
            entry_premium_bps: p.entry_premium_bps,
            exit_premium_bps: self.s.last_view.premium_bps,
            fees_usd: p.fees_usd,
            funding_usd: p.funding_usd,
            gross_pnl_usd: gross,
            pnl_usd: net,
            pnl_bps: if notional > 0.0 {
                net / notional * 1e4
            } else {
                0.0
            },
            exit_reason,
            exit_taker_sz: p.exit_taker_sz,
        };
        st.events.push(Event::TradeClosed(rec));
        if self.s.kill_reason.is_some() || matches!(self.s.phase, Phase::Halted { .. }) {
            self.halt();
        } else {
            self.s.phase = Phase::Flat;
            self.s.cooldown_until_ms = now.saturating_add(self.p.cooldown_ms);
        }
    }

    /// Run the machine until the phase is stable.
    fn advance(&mut self, now: u64, st: &mut Step) {
        for _ in 0..6 {
            let before = self.s.phase.kind();
            self.maybe_close_trade(now, st);
            match self.s.phase.kind() {
                PhaseKind::Flat => self.step_flat(now, st),
                PhaseKind::Entering => self.step_entering(now, st),
                PhaseKind::InPosition => self.step_in_position(now, st),
                PhaseKind::Exiting => self.step_exiting(now, st),
                PhaseKind::Halted => self.step_halted(now, st),
            }
            if self.s.phase.kind() == before {
                break;
            }
        }
    }

    fn step_halted(&mut self, now: u64, st: &mut Step) {
        self.cancel_working(now, st);
    }

    fn step_flat(&mut self, now: u64, st: &mut Step) {
        if self.s.working.is_some() {
            // Leftover order (e.g. exit order whose Done is pending).
            self.cancel_working(now, st);
            return;
        }
        if self.pos_size() > self.eps() {
            st.events.push(Event::Warning(
                "untracked position detected while flat; managing it".into(),
            ));
            self.s.phase = Phase::InPosition;
            return;
        }
        let v = self.s.last_view;
        if !self.s.entries_allowed || now < self.s.cooldown_until_ms || !v.usable() {
            return;
        }
        let Some(prem) = v.premium_bps else {
            return;
        };
        let Some(side) = entry_signal(prem, self.p.entry_threshold_bps) else {
            return;
        };
        let is_buy = side.open_is_buy();
        let Some(px) = self.maker_px(is_buy) else {
            return;
        };
        if !self.can_place(now) {
            return;
        }
        let plan = match plan_entry(
            &self.p.sizing,
            px,
            self.p.sz_decimals,
            self.p.asset_max_leverage,
            self.s.available_margin_usd,
        ) {
            Ok(p) => p,
            Err(e) => {
                st.events
                    .push(Event::Warning(format!("entry sizing failed: {e}")));
                self.s.cooldown_until_ms = now.saturating_add(self.p.entry_retry_cooldown_ms);
                return;
            }
        };
        self.s.phase = Phase::Entering {
            side,
            started_ms: now,
            timeout_at_ms: now.saturating_add(self.p.entry_timeout_ms),
            target_sz: plan.size,
            signal_premium_bps: prem,
            abort: None,
        };
        st.events.push(Event::EntryStarted {
            side,
            premium_bps: prem,
            px,
            size: plan.size,
            notional_usd: plan.notional_usd,
        });
        self.place(
            now,
            is_buy,
            px,
            plan.size,
            OrderKind::Alo,
            Purpose::Entry,
            st,
        );
    }

    fn step_entering(&mut self, now: u64, st: &mut Step) {
        let Phase::Entering {
            side,
            timeout_at_ms,
            target_sz,
            abort,
            ..
        } = self.s.phase.clone()
        else {
            return;
        };
        let eps = self.eps();
        let filled = self.s.position.as_ref().map_or(0.0, |p| p.entry_sz);
        let remaining = target_sz - filled;
        let v = self.s.last_view;
        let abort = abort.or_else(|| {
            if !self.s.entries_allowed {
                Some(AbortReason::EntriesBlocked)
            } else if now >= timeout_at_ms {
                Some(AbortReason::Timeout)
            } else if v.usable()
                && v.premium_bps
                    .is_some_and(|p| converged(side, p, self.p.exit_converge_bps))
            {
                Some(AbortReason::SignalGone)
            } else {
                None
            }
        });
        if let Phase::Entering { abort: a, .. } = &mut self.s.phase {
            a.clone_from(&abort);
        }

        if abort.is_some() || remaining < eps {
            if self.s.working.is_some() {
                self.cancel_working(now, st);
                return; // wait for Done (late fills are still counted)
            }
            self.finish_entry(now, abort, st);
            return;
        }

        match self.s.working.as_ref() {
            None => {
                if !v.usable() {
                    return;
                }
                let is_buy = side.open_is_buy();
                let Some(px) = self.maker_px(is_buy) else {
                    return;
                };
                let rem = round_size(remaining, self.p.sz_decimals, RoundDir::Down).ok();
                match rem {
                    Some(sz) if sz * px >= MIN_ORDER_NOTIONAL_USD => {
                        if self.can_place(now) {
                            self.place(now, is_buy, px, sz, OrderKind::Alo, Purpose::Entry, st);
                        }
                    }
                    // Remainder too small to quote: keep what we have.
                    _ => self.finish_entry(now, None, st),
                }
            }
            Some(_) => self.maybe_requote(now, st),
        }
    }

    fn finish_entry(&mut self, now: u64, abort: Option<AbortReason>, st: &mut Step) {
        let has_pos = self.pos_size() > self.eps();
        if has_pos {
            if let Some(p) = self.s.position.as_ref() {
                st.events.push(Event::PositionOpened {
                    side: p.side,
                    size: p.size,
                    avg_px: p.avg_entry_px().unwrap_or(0.0),
                    opened_ms: p.opened_ms,
                });
            }
            match abort {
                Some(AbortReason::Killed { flatten: true }) => {
                    self.s.phase = Phase::InPosition;
                    self.start_exit(now, ExitReason::Kill, st);
                }
                Some(AbortReason::Killed { flatten: false }) => self.halt(),
                _ => self.s.phase = Phase::InPosition,
            }
        } else {
            // Nothing (left) open. A position that was opened and closed
            // during the entry is handled by maybe_close_trade.
            self.s.position = None;
            match abort {
                Some(AbortReason::Killed { .. }) => self.halt(),
                other => {
                    st.events.push(Event::EntryAborted {
                        reason: other.unwrap_or(AbortReason::Timeout),
                    });
                    self.s.phase = Phase::Flat;
                    self.s.cooldown_until_ms = now.saturating_add(self.p.entry_retry_cooldown_ms);
                }
            }
        }
    }

    fn step_in_position(&mut self, now: u64, st: &mut Step) {
        let Some(pos) = self.s.position.as_ref() else {
            self.s.phase = Phase::Flat;
            return;
        };
        if pos.size < self.eps() {
            return; // closed by maybe_close_trade on the next pass
        }
        if self.s.working.is_some() {
            self.cancel_working(now, st);
            return;
        }
        let side = pos.side;
        let deadline = pos.deadline_ms(self.p.max_hold_ms);
        let v = self.s.last_view;
        let exit_start = deadline
            .saturating_sub(self.p.taker_fallback_window_ms)
            .saturating_sub(self.p.exit_maker_timeout_ms);
        if self.p.early_exit_on_converge
            && v.usable()
            && v.premium_bps
                .is_some_and(|p| converged(side, p, self.p.exit_converge_bps))
        {
            self.start_exit(now, ExitReason::Converged, st);
        } else if now >= exit_start {
            self.start_exit(now, ExitReason::MaxHold, st);
        }
    }

    fn step_exiting(&mut self, now: u64, st: &mut Step) {
        let Phase::Exiting {
            taker_at_ms,
            taker_announced,
            ..
        } = self.s.phase
        else {
            return;
        };
        let Some(pos) = self.s.position.as_ref() else {
            // Nothing to close (closed elsewhere): back to flat / halted.
            if self.s.kill_reason.is_some() {
                self.halt();
            } else {
                self.s.phase = Phase::Flat;
            }
            return;
        };
        let side = pos.side;
        let size = pos.size;
        if size < self.eps() {
            return;
        }
        let taker = now >= taker_at_ms;
        if let Some(w) = self.s.working.as_ref() {
            if w.cancel_requested {
                return;
            }
            if taker && w.intent.kind == OrderKind::Alo {
                self.cancel_working(now, st);
            } else if !taker {
                self.maybe_requote(now, st);
            }
            return;
        }
        if !self.s.last_view.usable() {
            return;
        }
        let is_buy = side.close_is_buy();
        let Ok(sz) = round_size(size, self.p.sz_decimals, RoundDir::Nearest) else {
            return;
        };
        if taker {
            let Some(px) = self.taker_px(is_buy) else {
                return;
            };
            if !self.can_place(now) {
                return;
            }
            if !taker_announced {
                st.events.push(Event::TakerFallback { remaining: sz });
                if let Phase::Exiting {
                    taker_announced, ..
                } = &mut self.s.phase
                {
                    *taker_announced = true;
                }
            }
            self.place(now, is_buy, px, sz, OrderKind::Ioc, Purpose::Exit, st);
        } else {
            let Some(px) = self.maker_px(is_buy) else {
                return;
            };
            if self.can_place(now) {
                self.place(now, is_buy, px, sz, OrderKind::Alo, Purpose::Exit, st);
            }
        }
    }
}

#[cfg(test)]
mod tests;
