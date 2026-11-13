//! Paper executor: live mainnet market data, simulated fills, fees from the
//! config. Never touches `/exchange` and never needs a key.
//!
//! Fill model ([`PaperSim`], pure and deterministic, also used by `replay`):
//!
//! * **ALO (post-only)**: rejected (`PostOnlyRejected`) if it would cross
//!   the book at placement: a buy at or above the best ask, a sell at or
//!   below the best bid. Otherwise it rests at its limit price.
//! * A resting order fills, as maker at its own price, when
//!   - the opposite side of the book reaches it (best ask <= our bid / best
//!     bid >= our ask): filled up to the opposite depth at prices that cross;
//!   - a public trade prints *through* our price (a trade below our bid /
//!     above our ask): filled up to the trade size;
//!   - a public trade prints exactly *at* our price with the aggressor on the
//!     other side (`fill_on_touch`, default true). With `queue_model`
//!     (default true) the order joins the back of its price level: the size
//!     shown at our price when we placed is ahead of us, trades at our price
//!     consume that queue first and only the excess fills us. The queue also
//!     shrinks to the level's current size when the level gets smaller
//!     (cancels ahead of us are assumed). With `fill_on_touch = false` only
//!     trades through count (pessimistic: always last in the queue).
//!
//!   Only trades stamped at or after the order's placement are used, and
//!   trades are de-duplicated by `tid` (the ws re-sends a snapshot of
//!   recent trades on every resubscription).
//! * **IOC (taker)**: walks the current book depth up to the limit price,
//!   one fill per level at the level price, taker fee; the unfilled
//!   remainder ends as `IocExpired`. Depth taken by an IOC is remembered
//!   until the next book update, so a retry does not fill twice against the
//!   same liquidity.
//! * **Reduce-only**: capped to the open position; with no position to
//!   reduce the order is canceled (like the exchange's reduceOnlyCanceled).
//! * **Funding**: settled at each UTC hour boundary on the open position at
//!   the oracle price: `-signed_size * oracle * funding_rate` (longs pay when
//!   the rate is positive).
//!
//! The simulated account (position, average entry, realized PnL, fees,
//! funding) is independent of the strategy's own trade accounting and is
//! persisted with the bot state.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::{ExecEvent, Executor, OrderView};
use crate::config::Config;
use crate::hl::market::MarketState;
use crate::hl::types::{Level, WsTrade};
use crate::hl::ws::UserEvent;
use crate::strategy::signal::{Command, DoneReason, OrderIntent, OrderKind, OrderUpdate};
use crate::strategy::sizing::lot_epsilon;

const HOUR_MS: u64 = 3_600_000;
const SEEN_TIDS_KEEP: usize = 512;

/// Paper fill model parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct PaperParams {
    pub maker_rate: f64,
    pub taker_rate: f64,
    pub fill_on_touch: bool,
    /// Track the queue ahead of a resting order at its price level.
    pub queue_model: bool,
    pub sz_decimals: u32,
    pub initial_balance_usd: f64,
    pub leverage: u32,
    /// Max age of a REST book to accept for a post-only check.
    pub stale_ms: u64,
}

impl PaperParams {
    pub fn from_config(cfg: &Config, sz_decimals: u32) -> Self {
        Self {
            maker_rate: cfg.fees.maker_rate,
            taker_rate: cfg.fees.taker_rate,
            fill_on_touch: cfg.paper.fill_on_touch,
            queue_model: cfg.paper.queue_model,
            sz_decimals,
            initial_balance_usd: cfg.paper.initial_balance_usd,
            leverage: cfg.sizing.leverage.max(1),
            stale_ms: cfg.timing.ws_stale_secs * 1000,
        }
    }
}

/// A simulated resting order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperOrder {
    pub tag: u64,
    pub is_buy: bool,
    pub px: f64,
    pub sz: f64,
    pub filled: f64,
    pub reduce_only: bool,
    pub placed_ms: u64,
    /// Size ahead of us at our price level (queue model).
    #[serde(default)]
    pub queue_ahead: f64,
}

impl PaperOrder {
    fn remaining(&self) -> f64 {
        (self.sz - self.filled).max(0.0)
    }
}

/// The simulated exchange account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PaperAccount {
    /// Signed position size (> 0 long).
    pub position_sz: f64,
    /// Average entry price of the open position (0 when flat).
    pub entry_px: f64,
    /// Realized price PnL (before fees and funding).
    pub realized_pnl_usd: f64,
    pub fees_usd: f64,
    /// Funding received (> 0) or paid (< 0).
    pub funding_usd: f64,
    pub fills: u64,
    pub volume_usd: f64,
    /// Last UTC hour seen by the funding clock.
    pub last_funding_hour: Option<u64>,
}

impl PaperAccount {
    /// Mark-to-market PnL of the open position.
    pub fn unrealized_pnl(&self, mark_px: f64) -> f64 {
        if self.position_sz == 0.0 || !mark_px.is_finite() {
            return 0.0;
        }
        self.position_sz * (mark_px - self.entry_px)
    }

    /// Net PnL since the start (realized - fees + funding + unrealized).
    pub fn net_pnl(&self, mark_px: Option<f64>) -> f64 {
        self.realized_pnl_usd - self.fees_usd
            + self.funding_usd
            + mark_px.map_or(0.0, |m| self.unrealized_pnl(m))
    }

    fn apply_fill(&mut self, is_buy: bool, px: f64, sz: f64, fee: f64, eps: f64) {
        let d = if is_buy { sz } else { -sz };
        let pos = self.position_sz;
        if pos.abs() < eps || pos.signum() == d.signum() {
            let new = pos.abs() + sz;
            self.entry_px = (self.entry_px * pos.abs() + px * sz) / new;
            self.position_sz = pos + d;
        } else {
            let close = sz.min(pos.abs());
            self.realized_pnl_usd += close * (px - self.entry_px) * pos.signum();
            let new = pos + d;
            if new.abs() < eps {
                self.position_sz = 0.0;
                self.entry_px = 0.0;
            } else {
                if new.signum() != pos.signum() {
                    self.entry_px = px; // flipped: the excess opens at px
                }
                self.position_sz = new;
            }
        }
        self.fees_usd += fee;
        self.fills += 1;
        self.volume_usd += px * sz;
    }
}

/// Persisted paper state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PaperState {
    pub orders: Vec<PaperOrder>,
    pub account: PaperAccount,
    /// Recently seen public trade ids (de-duplication across resubscriptions).
    pub seen_tids: VecDeque<u64>,
}

/// The pure paper fill model.
#[derive(Debug, Clone)]
pub struct PaperSim {
    p: PaperParams,
    s: PaperState,
    /// Receive time of the last book used for cross fills (a book is used
    /// once, so ctx-only updates do not re-fill against the same depth).
    last_book_recv_ms: Option<u64>,
    /// Depth already taken by IOCs from the book received at `.0`:
    /// (is a buy, level px, size taken).
    ioc_taken: (Option<u64>, Vec<(bool, f64, f64)>),
}

fn same_px(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn done(tag: u64, reason: DoneReason, filled_sz: f64) -> ExecEvent {
    ExecEvent::Order(OrderUpdate::Done {
        tag,
        reason,
        filled_sz: Some(filled_sz),
    })
}

/// Size shown on our side of the book at exactly `px`.
fn level_size(m: &MarketState, is_buy: bool, px: f64) -> f64 {
    let side = if is_buy { &m.bids } else { &m.asks };
    side.iter()
        .filter(|l| same_px(l.px, px))
        .map(|l| l.sz)
        .sum()
}

impl PaperSim {
    pub fn new(p: PaperParams) -> Self {
        Self::from_state(p, PaperState::default())
    }

    pub fn from_state(p: PaperParams, s: PaperState) -> Self {
        Self {
            p,
            s,
            last_book_recv_ms: None,
            ioc_taken: (None, Vec::new()),
        }
    }

    pub fn state(&self) -> &PaperState {
        &self.s
    }

    pub fn params(&self) -> &PaperParams {
        &self.p
    }

    pub fn account(&self) -> &PaperAccount {
        &self.s.account
    }

    pub fn open_orders(&self) -> &[PaperOrder] {
        &self.s.orders
    }

    fn eps(&self) -> f64 {
        lot_epsilon(self.p.sz_decimals)
    }

    /// Simulated account equity at `mark_px`.
    pub fn equity(&self, mark_px: Option<f64>) -> f64 {
        self.p.initial_balance_usd + self.s.account.net_pnl(mark_px)
    }

    /// Equity minus the isolated margin of the open position.
    pub fn available_margin(&self, mark_px: Option<f64>) -> f64 {
        let a = &self.s.account;
        let used = a.position_sz.abs() * a.entry_px / self.p.leverage.max(1) as f64;
        (self.equity(mark_px) - used).max(0.0)
    }

    /// Size a reduce-only order may still trade (0 = nothing to reduce).
    fn reduce_cap(&self, is_buy: bool) -> f64 {
        let pos = self.s.account.position_sz;
        let reduces = (is_buy && pos < 0.0) || (!is_buy && pos > 0.0);
        if reduces {
            pos.abs()
        } else {
            0.0
        }
    }

    fn fill_events(
        &mut self,
        tag: u64,
        is_buy: bool,
        px: f64,
        sz: f64,
        taker: bool,
        time_ms: u64,
    ) -> ExecEvent {
        let rate = if taker {
            self.p.taker_rate
        } else {
            self.p.maker_rate
        };
        let fee = px * sz * rate;
        let eps = self.eps();
        self.s.account.apply_fill(is_buy, px, sz, fee, eps);
        ExecEvent::Order(OrderUpdate::Filled {
            tag: Some(tag),
            is_buy,
            px,
            sz,
            fee_usd: fee,
            crossed: taker,
            time_ms,
        })
    }

    /// Execute one strategy command.
    pub fn execute(&mut self, now_ms: u64, cmd: Command, m: &MarketState) -> Vec<ExecEvent> {
        match cmd {
            Command::Place(o) => self.place(now_ms, o, m),
            Command::Cancel { tag } => self.cancel(tag),
        }
    }

    pub fn cancel(&mut self, tag: u64) -> Vec<ExecEvent> {
        match self.s.orders.iter().position(|o| o.tag == tag) {
            Some(i) => {
                let o = self.s.orders.remove(i);
                vec![done(tag, DoneReason::Canceled, o.filled)]
            }
            // Never placed, or already finished (its Done was sent then and
            // fills are synchronous here): answer anyway so a strategy that
            // missed the Done cannot wait forever. A duplicate is ignored.
            None => vec![done(
                tag,
                DoneReason::Rejected("order unknown to the executor".into()),
                0.0,
            )],
        }
    }

    /// The executor's view of an order (see [`OrderView`]).
    pub fn order_view(&self, tag: u64) -> OrderView {
        if self.s.orders.iter().any(|o| o.tag == tag) {
            OrderView::Active {
                cancel_requested: false,
            }
        } else {
            OrderView::Unknown
        }
    }

    pub fn place(&mut self, now_ms: u64, o: OrderIntent, m: &MarketState) -> Vec<ExecEvent> {
        let eps = self.eps();
        if !(o.px.is_finite() && o.px > 0.0 && o.sz.is_finite() && o.sz > eps) {
            return vec![done(
                o.tag,
                DoneReason::Rejected(format!("invalid order {} @ {}", o.sz, o.px)),
                0.0,
            )];
        }
        let mut sz = o.sz;
        if o.reduce_only {
            let cap = self.reduce_cap(o.is_buy);
            if cap < eps {
                return vec![done(
                    o.tag,
                    DoneReason::Rejected("reduce-only order with nothing to reduce".into()),
                    0.0,
                )];
            }
            sz = sz.min(cap);
        }
        match o.kind {
            OrderKind::Alo => {
                let (Some(bid), Some(ask)) = (m.best_bid, m.best_ask) else {
                    return vec![done(o.tag, DoneReason::Rejected("no book".into()), 0.0)];
                };
                if !m.book_fresh(now_ms, self.p.stale_ms) {
                    return vec![done(o.tag, DoneReason::Rejected("book stale".into()), 0.0)];
                }
                let crosses = if o.is_buy {
                    o.px >= ask.px
                } else {
                    o.px <= bid.px
                };
                if crosses {
                    return vec![done(o.tag, DoneReason::PostOnlyRejected, 0.0)];
                }
                let queue_ahead = if self.p.queue_model {
                    level_size(m, o.is_buy, o.px)
                } else {
                    0.0
                };
                self.s.orders.push(PaperOrder {
                    tag: o.tag,
                    is_buy: o.is_buy,
                    px: o.px,
                    sz,
                    filled: 0.0,
                    reduce_only: o.reduce_only,
                    placed_ms: now_ms,
                    queue_ahead,
                });
                vec![ExecEvent::Order(OrderUpdate::Resting { tag: o.tag })]
            }
            OrderKind::Ioc => {
                let levels: Vec<Level> = if o.is_buy {
                    m.asks.clone()
                } else {
                    m.bids.clone()
                };
                if self.ioc_taken.0 != m.book_recv_ms {
                    self.ioc_taken = (m.book_recv_ms, Vec::new());
                }
                let mut out = Vec::new();
                let mut rem = sz;
                for l in levels {
                    let ok = if o.is_buy {
                        l.px <= o.px + 1e-12
                    } else {
                        l.px >= o.px - 1e-12
                    };
                    if !ok || rem < eps {
                        break;
                    }
                    let taken: f64 = self
                        .ioc_taken
                        .1
                        .iter()
                        .filter(|(b, px, _)| *b == o.is_buy && same_px(*px, l.px))
                        .map(|(_, _, q)| q)
                        .sum();
                    let q = (l.sz - taken).max(0.0).min(rem);
                    if q < eps {
                        continue;
                    }
                    rem -= q;
                    self.ioc_taken.1.push((o.is_buy, l.px, q));
                    out.push(self.fill_events(o.tag, o.is_buy, l.px, q, true, now_ms));
                }
                let reason = if rem < eps {
                    DoneReason::Filled
                } else {
                    DoneReason::IocExpired
                };
                out.push(done(o.tag, reason, sz - rem));
                out
            }
        }
    }

    /// Fill resting order `i` by `q` at its price (maker); returns the
    /// events, removing the order when it is complete.
    fn fill_resting(&mut self, i: usize, q: f64, time_ms: u64, out: &mut Vec<ExecEvent>) {
        let eps = self.eps();
        let (tag, is_buy, px, reduce_only) = {
            let o = &self.s.orders[i];
            (o.tag, o.is_buy, o.px, o.reduce_only)
        };
        let mut q = q.min(self.s.orders[i].remaining());
        if reduce_only {
            q = q.min(self.reduce_cap(is_buy));
        }
        if q >= eps {
            self.s.orders[i].filled += q;
            out.push(self.fill_events(tag, is_buy, px, q, false, time_ms));
        }
        let o = &self.s.orders[i];
        let filled = o.filled;
        if o.remaining() < eps {
            self.s.orders.remove(i);
            out.push(done(tag, DoneReason::Filled, filled));
        } else if reduce_only && self.reduce_cap(is_buy) < eps {
            self.s.orders.remove(i);
            out.push(done(tag, DoneReason::Canceled, filled));
        }
    }

    /// Book update: resting orders crossed by the opposite side fill (up to
    /// the crossing depth); funding clock.
    pub fn on_book(&mut self, now_ms: u64, m: &MarketState) -> Vec<ExecEvent> {
        let mut out = self.funding_clock(now_ms, m);
        if m.book_recv_ms.is_none() || m.book_recv_ms == self.last_book_recv_ms {
            return out;
        }
        self.last_book_recv_ms = m.book_recv_ms;
        if self.p.queue_model {
            // The queue ahead cannot exceed what the level now shows.
            for k in 0..self.s.orders.len() {
                let (is_buy, px) = (self.s.orders[k].is_buy, self.s.orders[k].px);
                let shown = level_size(m, is_buy, px);
                let q = &mut self.s.orders[k].queue_ahead;
                *q = q.min(shown);
            }
        }
        let mut i = 0;
        while i < self.s.orders.len() {
            let o = &self.s.orders[i];
            let (is_buy, px) = (o.is_buy, o.px);
            let crossing: f64 = if is_buy {
                m.asks.iter().filter(|l| l.px <= px).map(|l| l.sz).sum()
            } else {
                m.bids.iter().filter(|l| l.px >= px).map(|l| l.sz).sum()
            };
            // Best level alone if the depth vectors are not populated.
            let crossing = if crossing > 0.0 {
                crossing
            } else {
                match (is_buy, m.best_ask, m.best_bid) {
                    (true, Some(a), _) if a.px <= px => a.sz,
                    (false, _, Some(b)) if b.px >= px => b.sz,
                    _ => 0.0,
                }
            };
            let before = self.s.orders.len();
            if crossing > 0.0 {
                self.fill_resting(i, crossing, now_ms, &mut out);
            }
            if self.s.orders.len() == before {
                i += 1;
            }
        }
        out
    }

    /// Public trades: trade-through (and touch) fills.
    pub fn on_trades(&mut self, trades: &[WsTrade]) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        for t in trades {
            if t.tid != 0 {
                if self.s.seen_tids.contains(&t.tid) {
                    continue;
                }
                self.s.seen_tids.push_back(t.tid);
                while self.s.seen_tids.len() > SEEN_TIDS_KEEP {
                    self.s.seen_tids.pop_front();
                }
            }
            if !(t.px.is_finite() && t.sz.is_finite() && t.sz > 0.0) {
                continue;
            }
            let mut avail = t.sz;
            let mut i = 0;
            while i < self.s.orders.len() && avail > 0.0 {
                let o = &self.s.orders[i];
                let through = if o.is_buy { t.px < o.px } else { t.px > o.px };
                let touch = self.p.fill_on_touch
                    && same_px(t.px, o.px)
                    && (t.buyer_aggressor() != o.is_buy);
                let hits = t.time >= o.placed_ms && (through || touch);
                let before = self.s.orders.len();
                if hits {
                    if !through && self.p.queue_model {
                        // A print at our price serves the queue ahead first.
                        let o = &mut self.s.orders[i];
                        let eat = avail.min(o.queue_ahead);
                        o.queue_ahead -= eat;
                        avail -= eat;
                    }
                    let q = avail.min(self.s.orders[i].remaining());
                    if q > 0.0 {
                        avail -= q;
                        self.fill_resting(i, q, t.time, &mut out);
                    }
                }
                if self.s.orders.len() == before {
                    i += 1;
                }
            }
        }
        out
    }

    /// Settle funding when a UTC hour boundary is crossed.
    pub fn funding_clock(&mut self, now_ms: u64, m: &MarketState) -> Vec<ExecEvent> {
        let hour = now_ms / HOUR_MS;
        let prev = self.s.account.last_funding_hour.replace(hour);
        let Some(prev) = prev else {
            return Vec::new();
        };
        if hour <= prev || self.s.account.position_sz == 0.0 {
            return Vec::new();
        }
        let (Some(rate), Some(oracle)) = (m.funding, m.oracle_px) else {
            return Vec::new();
        };
        let usd = -self.s.account.position_sz * oracle * rate;
        if !usd.is_finite() {
            return Vec::new();
        }
        self.s.account.funding_usd += usd;
        vec![ExecEvent::Funding {
            usd,
            time_ms: hour * HOUR_MS,
        }]
    }

    /// Cancel every resting order.
    pub fn cancel_all(&mut self) -> Vec<ExecEvent> {
        self.s
            .orders
            .drain(..)
            .map(|o| done(o.tag, DoneReason::Canceled, o.filled))
            .collect()
    }
}

/// [`Executor`] wrapper around [`PaperSim`].
pub struct PaperExecutor {
    sim: PaperSim,
    last_mark: Option<f64>,
}

impl PaperExecutor {
    pub fn new(sim: PaperSim) -> Self {
        Self {
            sim,
            last_mark: None,
        }
    }

    pub fn sim(&self) -> &PaperSim {
        &self.sim
    }

    /// Restore from the persisted JSON (or start fresh).
    pub fn from_saved(p: PaperParams, saved: Option<serde_json::Value>) -> anyhow::Result<Self> {
        let s = match saved {
            Some(v) if !v.is_null() => serde_json::from_value::<PaperState>(v)?,
            _ => PaperState::default(),
        };
        Ok(Self::new(PaperSim::from_state(p, s)))
    }
}

impl Executor for PaperExecutor {
    fn kind(&self) -> &'static str {
        "paper"
    }

    async fn execute(
        &mut self,
        now_ms: u64,
        cmds: Vec<Command>,
        market: &MarketState,
    ) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        for c in cmds {
            tracing::info!(cmd = ?c, "paper order");
            out.extend(self.sim.execute(now_ms, c, market));
        }
        out
    }

    fn on_market(&mut self, now_ms: u64, market: &MarketState) -> Vec<ExecEvent> {
        if market.mark_px.is_some() {
            self.last_mark = market.mark_px;
        }
        self.sim.on_book(now_ms, market)
    }

    fn on_trades(&mut self, _now_ms: u64, trades: &[WsTrade]) -> Vec<ExecEvent> {
        self.sim.on_trades(trades)
    }

    async fn on_user_event(&mut self, _now_ms: u64, _ev: UserEvent) -> Vec<ExecEvent> {
        Vec::new()
    }

    async fn reconcile(&mut self, _now_ms: u64, _force: bool) -> Vec<ExecEvent> {
        Vec::new()
    }

    fn available_margin_usd(&self) -> Option<f64> {
        Some(self.sim.available_margin(self.last_mark))
    }

    fn venue_position(&self) -> Option<f64> {
        Some(self.sim.account().position_sz)
    }

    fn open_order_count(&self) -> usize {
        self.sim.open_orders().len()
    }

    fn order_view(&self, tag: u64) -> OrderView {
        self.sim.order_view(tag)
    }

    fn save_state(&self) -> serde_json::Value {
        serde_json::to_value(self.sim.state()).unwrap_or(serde_json::Value::Null)
    }

    async fn shutdown(&mut self, _now_ms: u64, _market: &MarketState) -> Vec<ExecEvent> {
        // Paper orders are part of the persisted state and resume on restart
        // together with the strategy's working order: nothing to cancel.
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
