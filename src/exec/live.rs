//! Live executor (testnet / guarded mainnet).
//!
//! * Orders go through the signing [`ExchangeClient`]; every order carries a
//!   cloid, the strategy `tag` <-> cloid / oid map is persisted.
//! * Feedback: the `order` response (resting / filled / error), then the
//!   websocket `orderUpdates` (terminal states) and `userFills` (fills,
//!   de-duplicated by `tid`). Fills are only taken from userFills (never
//!   from the order response) so nothing is counted twice.
//! * Uncertain outcomes (transport error, 5xx, undecodable answer) mark the
//!   order `uncertain`; the next reconciliation resolves it with
//!   `orderStatus` by cloid. A dropped websocket user event forces a full
//!   resync (REST `userFills` + `openOrders`).
//! * Startup reconciliation: `openOrders{dex}` (own persisted orders are
//!   adopted, unknown ones canceled or kept per `live.stray_orders`), status
//!   of persisted orders that are no longer open, `userFills` missed while
//!   down, `clearinghouseState{dex}` for the position (adopted by the runner
//!   per `live.adopt_position`) and isolated leverage.
//! * Rate limits: a client-side cap on actions per rolling minute
//!   (`live.max_exchange_actions_per_min`, cancels are never blocked) and a
//!   pause of `live.rate_limit_backoff_secs` after any rate-limit answer.
//! * Shutdown: cancel the bot's open orders (`live.cancel_on_shutdown`),
//!   keep the position unless `live.flatten_on_shutdown`.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ExecEvent, Executor};
use crate::config::{Config, LiveConfig, StrayOrders};
use crate::hl::actions::Cloid;
use crate::hl::exchange::{
    AssetRef, CancelStatus, CloidGen, ExchangeClient, ExchangeError, OrderRequest, OrderStatus,
};
use crate::hl::info::InfoClient;
use crate::hl::market::MarketState;
use crate::hl::rounding::{round_price, RoundDir};
use crate::hl::types::{ClearinghouseState, Fill, OpenOrder, WsTrade};
use crate::hl::ws::UserEvent;
use crate::strategy::premium::from_bps;
use crate::strategy::signal::{Command, DoneReason, OrderIntent, OrderKind, OrderUpdate};
use crate::strategy::sizing::lot_epsilon;

const MINUTE_MS: u64 = 60_000;
const SEEN_TIDS_KEEP: usize = 4_000;
const DONE_ORDERS_KEEP: usize = 32;
/// An order unknown to the exchange this long after placement is dropped.
const UNKNOWN_ORDER_GRACE_MS: u64 = 10_000;

/// Exchange status of one order (`orderStatus`, `orderUpdates`).
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteStatus {
    Unknown,
    Open,
    Filled,
    Canceled(String),
    Rejected(String),
}

impl RemoteStatus {
    /// Map an exchange status string (orderUpdates / orderStatus).
    pub fn from_status(s: &str) -> Self {
        match s {
            "open" | "triggered" => RemoteStatus::Open,
            "filled" => RemoteStatus::Filled,
            x if x.to_ascii_lowercase().ends_with("rejected") => RemoteStatus::Rejected(x.into()),
            x => RemoteStatus::Canceled(x.into()),
        }
    }

    /// Parse an `orderStatus` info response.
    pub fn from_order_status_json(v: &Value) -> Self {
        match v.get("status").and_then(Value::as_str) {
            Some("order") => v
                .pointer("/order/status")
                .and_then(Value::as_str)
                .map_or(RemoteStatus::Unknown, RemoteStatus::from_status),
            _ => RemoteStatus::Unknown,
        }
    }

    /// Terminal reason for the strategy (`None` while open / unknown).
    fn done_reason(&self, kind: OrderKind) -> Option<DoneReason> {
        match self {
            RemoteStatus::Unknown | RemoteStatus::Open => None,
            RemoteStatus::Filled => Some(DoneReason::Filled),
            RemoteStatus::Canceled(_) if kind == OrderKind::Ioc => Some(DoneReason::IocExpired),
            RemoteStatus::Canceled(_) => Some(DoneReason::Canceled),
            RemoteStatus::Rejected(s) if s == "badAloPxRejected" => {
                Some(DoneReason::PostOnlyRejected)
            }
            RemoteStatus::Rejected(s) if s == "iocCancelRejected" => Some(DoneReason::IocExpired),
            RemoteStatus::Rejected(s) => Some(DoneReason::Rejected(s.clone())),
        }
    }
}

/// What the live executor needs from Hyperliquid (abstracted for tests).
#[allow(async_fn_in_trait)]
pub trait LiveApi {
    async fn place(&self, req: &OrderRequest) -> Result<OrderStatus, ExchangeError>;
    async fn cancel_by_cloid(&self, cloid: Cloid) -> Result<CancelStatus, ExchangeError>;
    async fn update_leverage_isolated(&self, leverage: u32) -> Result<(), ExchangeError>;
    async fn clearinghouse(&self) -> anyhow::Result<ClearinghouseState>;
    async fn open_orders(&self) -> anyhow::Result<Vec<OpenOrder>>;
    async fn user_fills(&self) -> anyhow::Result<Vec<Fill>>;
    async fn order_status(&self, cloid: Cloid) -> anyhow::Result<RemoteStatus>;
    /// Funding payments on the coin since `start_ms`: (time ms, usd).
    async fn funding_since(&self, start_ms: u64) -> anyhow::Result<Vec<(u64, f64)>>;
}

/// [`LiveApi`] over the real exchange and info clients.
pub struct HlApi {
    pub exchange: ExchangeClient,
    pub info: InfoClient,
    pub asset: AssetRef,
    pub dex: String,
    pub coin: String,
    /// Account address for queries (not the agent wallet).
    pub user: String,
}

impl LiveApi for HlApi {
    async fn place(&self, req: &OrderRequest) -> Result<OrderStatus, ExchangeError> {
        self.exchange.order(self.asset, req).await
    }
    async fn cancel_by_cloid(&self, cloid: Cloid) -> Result<CancelStatus, ExchangeError> {
        self.exchange.cancel_by_cloid(self.asset, cloid).await
    }
    async fn update_leverage_isolated(&self, leverage: u32) -> Result<(), ExchangeError> {
        self.exchange
            .update_leverage_isolated(self.asset, leverage)
            .await
    }
    async fn clearinghouse(&self) -> anyhow::Result<ClearinghouseState> {
        self.info.clearinghouse_state(&self.user, &self.dex).await
    }
    async fn open_orders(&self) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(self
            .info
            .open_orders(&self.user, &self.dex)
            .await?
            .into_iter()
            .filter(|o| o.coin == self.coin)
            .collect())
    }
    async fn user_fills(&self) -> anyhow::Result<Vec<Fill>> {
        Ok(self
            .info
            .user_fills(&self.user)
            .await?
            .into_iter()
            .filter(|f| f.coin == self.coin)
            .collect())
    }
    async fn order_status(&self, cloid: Cloid) -> anyhow::Result<RemoteStatus> {
        let v = self
            .info
            .order_status(&self.user, Value::String(cloid.to_string()))
            .await?;
        Ok(RemoteStatus::from_order_status_json(&v))
    }
    async fn funding_since(&self, start_ms: u64) -> anyhow::Result<Vec<(u64, f64)>> {
        let v = self.info.user_funding(&self.user, start_ms).await?;
        Ok(parse_user_funding(&v, &self.coin))
    }
}

/// Extract `(time, usdc)` funding entries for `coin`.
pub fn parse_user_funding(entries: &[Value], coin: &str) -> Vec<(u64, f64)> {
    entries
        .iter()
        .filter_map(|e| {
            let d = e.get("delta")?;
            if d.get("type")?.as_str()? != "funding" || d.get("coin")?.as_str()? != coin {
                return None;
            }
            let usd = match d.get("usdc")? {
                Value::String(s) => s.parse().ok()?,
                Value::Number(n) => n.as_f64()?,
                _ => return None,
            };
            Some((e.get("time")?.as_u64()?, usd))
        })
        .collect()
}

/// One order the bot placed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveOrder {
    pub tag: u64,
    pub cloid: Cloid,
    pub oid: Option<u64>,
    pub is_buy: bool,
    pub px: f64,
    pub sz: f64,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub placed_ms: u64,
    /// Acknowledged as resting.
    pub acked: bool,
    /// Terminal `Done` already sent to the strategy.
    pub done: bool,
    /// Outcome of the place / cancel request unknown: resolve by cloid.
    pub uncertain: bool,
    /// The strategy asked to cancel it (retried by reconciliation while
    /// the order is still open).
    #[serde(default)]
    pub cancel_requested: bool,
}

/// Persisted live executor state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LiveState {
    pub orders: Vec<LiveOrder>,
    pub seen_tids: VecDeque<u64>,
    /// Time of the last fill processed (ms).
    pub last_fill_ms: u64,
    /// Time of the last funding payment processed (ms).
    pub last_funding_ms: u64,
}

#[derive(Debug, Clone)]
pub struct LiveParams {
    pub live: LiveConfig,
    /// Traded coin (clearinghouseState{dex} lists every coin of the dex).
    pub coin: String,
    pub leverage: u32,
    pub sz_decimals: u32,
    pub taker_max_slippage_bps: f64,
}

impl LiveParams {
    pub fn from_config(cfg: &Config, sz_decimals: u32) -> Self {
        Self {
            live: cfg.live.clone(),
            coin: cfg.market.coin.clone(),
            leverage: cfg.sizing.leverage.max(1),
            sz_decimals,
            taker_max_slippage_bps: cfg.strategy.taker_max_slippage_bps,
        }
    }
}

/// Position on the venue at startup / reconciliation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VenuePosition {
    /// Signed size (> 0 long).
    pub szi: f64,
    pub entry_px: Option<f64>,
}

pub struct LiveExecutor<A: LiveApi> {
    p: LiveParams,
    api: A,
    s: LiveState,
    cloids: CloidGen,
    started_ms: u64,
    action_times: VecDeque<u64>,
    paused_until_ms: u64,
    available_margin: Option<f64>,
    venue: Option<VenuePosition>,
}

/// `oo` (from openOrders) is our order `o`: by cloid when it has one.
fn is_listed(o: &LiveOrder, oo: &OpenOrder) -> bool {
    match oo.cloid.as_deref().and_then(|c| c.parse::<Cloid>().ok()) {
        Some(c) => c == o.cloid,
        None => o.oid == Some(oo.oid),
    }
}

fn done(tag: u64, reason: DoneReason) -> ExecEvent {
    ExecEvent::Order(OrderUpdate::Done { tag, reason })
}

impl<A: LiveApi> LiveExecutor<A> {
    pub fn new(p: LiveParams, api: A, saved: Option<Value>, now_ms: u64) -> anyhow::Result<Self> {
        let s = match saved {
            Some(v) if !v.is_null() => serde_json::from_value::<LiveState>(v)?,
            _ => LiveState::default(),
        };
        Ok(Self {
            p,
            api,
            s,
            cloids: CloidGen::default(),
            started_ms: now_ms,
            action_times: VecDeque::new(),
            paused_until_ms: 0,
            available_margin: None,
            venue: None,
        })
    }

    pub fn api(&self) -> &A {
        &self.api
    }

    pub fn state(&self) -> &LiveState {
        &self.s
    }

    /// Last known venue position (`None` before the first reconciliation).
    pub fn venue(&self) -> Option<VenuePosition> {
        self.venue
    }

    fn eps(&self) -> f64 {
        lot_epsilon(self.p.sz_decimals)
    }

    /// For the `OrderRequest` builders, which only use `sz_decimals`; the
    /// real asset id is applied by [`HlApi::place`].
    fn asset(&self) -> AssetRef {
        AssetRef {
            asset_id: 0,
            sz_decimals: self.p.sz_decimals,
        }
    }

    fn active(&self) -> impl Iterator<Item = &LiveOrder> {
        self.s.orders.iter().filter(|o| !o.done)
    }

    /// Our order for an exchange (cloid, oid): by cloid when present,
    /// else by oid.
    fn find_idx(&self, cloid: Option<&str>, oid: Option<u64>) -> Option<usize> {
        match cloid.and_then(|c| c.parse::<Cloid>().ok()) {
            Some(c) => self.s.orders.iter().position(|o| o.cloid == c),
            None => self
                .s
                .orders
                .iter()
                .position(|o| oid.is_some() && o.oid == oid),
        }
    }

    fn prune(&mut self, now_ms: u64) {
        while self
            .action_times
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= MINUTE_MS)
        {
            self.action_times.pop_front();
        }
        // Keep the active orders and the most recent finished ones.
        let done_count = self.s.orders.iter().filter(|o| o.done).count();
        if done_count > DONE_ORDERS_KEEP {
            let mut drop = done_count - DONE_ORDERS_KEEP;
            self.s.orders.retain(|o| {
                if o.done && drop > 0 {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    fn rate_limited(&mut self, now_ms: u64, what: &str, out: &mut Vec<ExecEvent>) {
        self.paused_until_ms = now_ms + self.p.live.rate_limit_backoff_secs * 1000;
        out.push(ExecEvent::Alert(format!(
            "exchange rate limit hit ({what}); new orders paused for {} s",
            self.p.live.rate_limit_backoff_secs
        )));
    }

    fn mark_done(&mut self, i: usize, reason: DoneReason, out: &mut Vec<ExecEvent>) {
        let o = &mut self.s.orders[i];
        if o.done {
            return;
        }
        o.done = true;
        o.uncertain = false;
        out.push(done(o.tag, reason));
    }

    async fn place(&mut self, now_ms: u64, o: OrderIntent) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        self.prune(now_ms);
        if now_ms < self.paused_until_ms {
            out.push(done(
                o.tag,
                DoneReason::Rejected("paused after a rate-limit answer".into()),
            ));
            return out;
        }
        if self.action_times.len() as u32 >= self.p.live.max_exchange_actions_per_min {
            out.push(done(
                o.tag,
                DoneReason::Rejected("local exchange action budget exhausted".into()),
            ));
            out.push(ExecEvent::Alert(format!(
                "local action budget ({} / min) exhausted: order {} refused",
                self.p.live.max_exchange_actions_per_min, o.tag
            )));
            return out;
        }
        let cloid = self.cloids.next_at(now_ms);
        let built = match o.kind {
            OrderKind::Alo => OrderRequest::alo(
                self.asset(),
                o.is_buy,
                o.px,
                o.sz,
                o.reduce_only,
                Some(cloid),
            ),
            OrderKind::Ioc => OrderRequest::ioc(
                self.asset(),
                o.is_buy,
                o.px,
                o.sz,
                o.reduce_only,
                Some(cloid),
            ),
        };
        let req = match built {
            Ok(r) => r,
            Err(e) => {
                out.push(done(o.tag, DoneReason::Rejected(e.to_string())));
                return out;
            }
        };
        self.s.orders.push(LiveOrder {
            tag: o.tag,
            cloid,
            oid: None,
            is_buy: o.is_buy,
            px: req.px,
            sz: req.sz,
            kind: o.kind,
            reduce_only: o.reduce_only,
            placed_ms: now_ms,
            acked: false,
            done: false,
            uncertain: false,
            cancel_requested: false,
        });
        let i = self.s.orders.len() - 1;
        self.action_times.push_back(now_ms);
        tracing::info!(tag = o.tag, cloid = %cloid, is_buy = o.is_buy, px = req.px, sz = req.sz, kind = ?o.kind, reduce_only = o.reduce_only, "placing order");
        match self.api.place(&req).await {
            Ok(OrderStatus::Resting { oid, .. }) => {
                let ord = &mut self.s.orders[i];
                ord.oid = Some(oid);
                if !ord.done && !ord.acked {
                    ord.acked = true;
                    out.push(ExecEvent::Order(OrderUpdate::Resting { tag: o.tag }));
                }
            }
            Ok(OrderStatus::Filled { oid, total_sz, .. }) => {
                self.s.orders[i].oid = Some(oid);
                let partial = total_sz.is_finite() && total_sz + self.eps() < req.sz;
                let reason = if partial && o.kind == OrderKind::Ioc {
                    DoneReason::IocExpired
                } else {
                    DoneReason::Filled
                };
                self.mark_done(i, reason, &mut out);
            }
            Ok(st @ OrderStatus::Error(_)) => {
                let reason = if st.is_post_only_reject() {
                    DoneReason::PostOnlyRejected
                } else if st.is_ioc_no_match() {
                    DoneReason::IocExpired
                } else {
                    if st.is_rate_limited() {
                        self.rate_limited(now_ms, "order", &mut out);
                    }
                    let OrderStatus::Error(m) = st else {
                        unreachable!()
                    };
                    DoneReason::Rejected(m)
                };
                self.mark_done(i, reason, &mut out);
            }
            Ok(OrderStatus::Other(v)) => {
                tracing::warn!(tag = o.tag, status = %v, "unexpected order status, resolving by cloid");
                self.s.orders[i].uncertain = true;
            }
            Err(e) if e.is_rate_limited() => {
                self.rate_limited(now_ms, "order", &mut out);
                self.mark_done(i, DoneReason::Rejected(e.to_string()), &mut out);
            }
            Err(e) if e.outcome_unknown() || matches!(e, ExchangeError::Decode(_)) => {
                self.s.orders[i].uncertain = true;
                out.push(ExecEvent::Alert(format!(
                    "order {} outcome unknown ({e}); reconciling by cloid",
                    o.tag
                )));
            }
            Err(e) => self.mark_done(i, DoneReason::Rejected(e.to_string()), &mut out),
        }
        out
    }

    async fn cancel(&mut self, now_ms: u64, tag: u64) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        let Some(i) = self.s.orders.iter().position(|o| o.tag == tag && !o.done) else {
            return out;
        };
        self.prune(now_ms);
        self.action_times.push_back(now_ms); // counted, never blocked
        self.s.orders[i].cancel_requested = true;
        let cloid = self.s.orders[i].cloid;
        match self.api.cancel_by_cloid(cloid).await {
            Ok(CancelStatus::Success) => self.mark_done(i, DoneReason::Canceled, &mut out),
            Ok(other) => {
                // Already filled / canceled / never placed: ask the exchange.
                tracing::info!(tag, status = ?other, "cancel not applied, checking order status");
                out.extend(self.resolve(now_ms, i).await);
            }
            Err(e) => {
                if e.is_rate_limited() {
                    self.rate_limited(now_ms, "cancel", &mut out);
                }
                self.s.orders[i].uncertain = true;
                out.push(ExecEvent::Alert(format!(
                    "cancel of order {tag} failed ({e}); will retry via reconciliation"
                )));
            }
        }
        out
    }

    /// Resolve one order's state with `orderStatus`.
    async fn resolve(&mut self, now_ms: u64, i: usize) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        let (cloid, kind, placed, tag) = {
            let o = &self.s.orders[i];
            (o.cloid, o.kind, o.placed_ms, o.tag)
        };
        match self.api.order_status(cloid).await {
            Ok(RemoteStatus::Open) => {
                let o = &mut self.s.orders[i];
                o.uncertain = false;
                if !o.acked {
                    o.acked = true;
                    out.push(ExecEvent::Order(OrderUpdate::Resting { tag }));
                }
            }
            Ok(RemoteStatus::Unknown) => {
                if now_ms.saturating_sub(placed) >= UNKNOWN_ORDER_GRACE_MS {
                    self.mark_done(
                        i,
                        DoneReason::Rejected("order unknown to the exchange".into()),
                        &mut out,
                    );
                } else {
                    self.s.orders[i].uncertain = true;
                }
            }
            Ok(st) => {
                if let Some(r) = st.done_reason(kind) {
                    self.mark_done(i, r, &mut out);
                }
            }
            Err(e) => {
                self.s.orders[i].uncertain = true;
                tracing::warn!(tag, error = %format!("{e:#}"), "orderStatus failed");
            }
        }
        out
    }

    /// Process fills (ws or REST). Fills of unknown orders older than the
    /// executor start are historical and skipped.
    fn process_fills(&mut self, fills: &[Fill]) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        let mut sorted: Vec<&Fill> = fills.iter().collect();
        sorted.sort_by_key(|f| (f.time, f.tid));
        for f in sorted {
            if self.s.seen_tids.contains(&f.tid) {
                continue;
            }
            let idx = self.find_idx(f.cloid.as_deref(), Some(f.oid));
            // Unknown orders: only fills newer than what we already handled
            // (or than the first start) count; older ones are history
            // replayed by the ws snapshot / REST userFills.
            let cutoff = if self.s.last_fill_ms > 0 {
                self.s.last_fill_ms
            } else {
                self.started_ms
            };
            if idx.is_none() && f.time < cutoff {
                continue;
            }
            self.s.seen_tids.push_back(f.tid);
            while self.s.seen_tids.len() > SEEN_TIDS_KEEP {
                self.s.seen_tids.pop_front();
            }
            self.s.last_fill_ms = self.s.last_fill_ms.max(f.time);
            let tag = idx.map(|i| self.s.orders[i].tag);
            if tag.is_none() {
                out.push(ExecEvent::Alert(format!(
                    "fill for an order the bot did not place: {} {} @ {} (oid {})",
                    f.side, f.sz, f.px, f.oid
                )));
            }
            out.push(ExecEvent::Order(OrderUpdate::Filled {
                tag,
                is_buy: f.is_buy(),
                px: f.px,
                sz: f.sz,
                fee_usd: f.fee + f.builder_fee.unwrap_or(0.0),
                crossed: f.crossed,
                time_ms: f.time,
            }));
        }
        out
    }

    fn process_order_updates(&mut self, ups: &[crate::hl::types::OrderUpdate]) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        for u in ups {
            let Some(i) = self.find_idx(u.order.cloid.as_deref(), Some(u.order.oid)) else {
                continue;
            };
            if self.s.orders[i].oid.is_none() {
                self.s.orders[i].oid = Some(u.order.oid);
            }
            let st = RemoteStatus::from_status(&u.status);
            if st == RemoteStatus::Open {
                let o = &mut self.s.orders[i];
                o.uncertain = false;
                if !o.acked && !o.done {
                    o.acked = true;
                    out.push(ExecEvent::Order(OrderUpdate::Resting { tag: o.tag }));
                }
            } else if let Some(r) = st.done_reason(self.s.orders[i].kind) {
                self.mark_done(i, r, &mut out);
            }
        }
        out
    }

    async fn refresh_account(&mut self) -> anyhow::Result<()> {
        let ch = self.api.clearinghouse().await?;
        self.available_margin = ch.withdrawable.or(Some(
            ch.margin_summary.account_value - ch.margin_summary.total_margin_used,
        ));
        let coin_pos = ch.position(&self.p.coin);
        self.venue = Some(match coin_pos {
            Some(p) => VenuePosition {
                szi: p.szi,
                entry_px: p.entry_px,
            },
            None => VenuePosition {
                szi: 0.0,
                entry_px: None,
            },
        });
        Ok(())
    }

    /// Startup reconciliation. The caller feeds the events to the engine,
    /// then compares [`Self::venue_position`] with the strategy position.
    pub async fn startup(&mut self, now_ms: u64) -> anyhow::Result<Vec<ExecEvent>> {
        let mut out = Vec::new();
        self.refresh_account().await?;
        // Open orders: adopt ours, handle strays.
        let open = self.api.open_orders().await?;
        for oo in &open {
            match self.find_idx(oo.cloid.as_deref(), Some(oo.oid)) {
                Some(i) if !self.s.orders[i].done => {
                    let o = &mut self.s.orders[i];
                    o.oid = Some(oo.oid);
                    o.uncertain = false;
                    if !o.acked {
                        o.acked = true;
                        out.push(ExecEvent::Order(OrderUpdate::Resting { tag: o.tag }));
                    }
                    tracing::info!(tag = o.tag, oid = oo.oid, "adopted own open order");
                }
                _ => match self.p.live.stray_orders {
                    StrayOrders::Cancel => {
                        let r = match oo.cloid.as_deref().and_then(|c| c.parse::<Cloid>().ok()) {
                            Some(c) => self.api.cancel_by_cloid(c).await.map(|_| ()),
                            None => Err(ExchangeError::InvalidOrder(
                                "stray order without cloid".into(),
                            )),
                        };
                        let msg = match r {
                            Ok(()) => format!(
                                "canceled stray open order oid {} ({} {} @ {})",
                                oo.oid, oo.side, oo.sz, oo.limit_px
                            ),
                            Err(e) => format!(
                                "could not cancel stray order oid {} ({e}): cancel it manually",
                                oo.oid
                            ),
                        };
                        out.push(ExecEvent::Alert(msg));
                    }
                    StrayOrders::Keep => out.push(ExecEvent::Alert(format!(
                        "stray open order oid {} ({} {} @ {}) left untouched",
                        oo.oid, oo.side, oo.sz, oo.limit_px
                    ))),
                },
            }
        }
        // Persisted orders that are not open any more.
        let gone: Vec<usize> = (0..self.s.orders.len())
            .filter(|&i| {
                let o = &self.s.orders[i];
                !o.done && !open.iter().any(|oo| is_listed(o, oo))
            })
            .collect();
        // Fills first (missed while down), then terminal states.
        let fills = self.api.user_fills().await?;
        out.extend(self.process_fills(&fills));
        for i in gone {
            out.extend(self.resolve(now_ms, i).await);
            if !self.s.orders[i].done {
                // Not open and status unknown: treat as gone.
                self.mark_done(
                    i,
                    DoneReason::Rejected("not open at startup".into()),
                    &mut out,
                );
            }
        }
        // Isolated leverage (only possible without an open position).
        if self.venue.is_some_and(|v| v.szi == 0.0) {
            if let Err(e) = self.api.update_leverage_isolated(self.p.leverage).await {
                anyhow::bail!("updateLeverage (isolated {}x) failed: {e}", self.p.leverage);
            }
        } else {
            out.push(ExecEvent::Alert(
                "position open at startup: leverage / margin mode left unchanged".into(),
            ));
        }
        if self.s.last_funding_ms == 0 {
            self.s.last_funding_ms = now_ms;
        }
        Ok(out)
    }
}

impl<A: LiveApi> Executor for LiveExecutor<A> {
    fn kind(&self) -> &'static str {
        "live"
    }

    async fn execute(
        &mut self,
        now_ms: u64,
        cmds: Vec<Command>,
        _market: &MarketState,
    ) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        for c in cmds {
            match c {
                Command::Place(o) => out.extend(self.place(now_ms, o).await),
                Command::Cancel { tag } => out.extend(self.cancel(now_ms, tag).await),
            }
        }
        out
    }

    fn on_market(&mut self, _now_ms: u64, _market: &MarketState) -> Vec<ExecEvent> {
        Vec::new()
    }

    fn on_trades(&mut self, _now_ms: u64, _trades: &[WsTrade]) -> Vec<ExecEvent> {
        Vec::new()
    }

    async fn on_user_event(&mut self, _now_ms: u64, ev: UserEvent) -> Vec<ExecEvent> {
        match ev {
            UserEvent::Fills { fills, .. } => self.process_fills(&fills),
            UserEvent::OrderUpdates(u) => self.process_order_updates(&u),
        }
    }

    async fn reconcile(&mut self, now_ms: u64, force: bool) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        if let Err(e) = self.refresh_account().await {
            tracing::warn!(error = %format!("{e:#}"), "clearinghouseState failed");
        }
        if force {
            match self.api.user_fills().await {
                Ok(f) => out.extend(self.process_fills(&f)),
                Err(e) => out.push(ExecEvent::Alert(format!("userFills resync failed: {e:#}"))),
            }
        }
        // Orders that need checking: uncertain ones, and on a forced resync
        // every active order missing from openOrders.
        let check_open = force || self.active().any(|o| o.uncertain);
        if check_open {
            match self.api.open_orders().await {
                Ok(open) => {
                    let idxs: Vec<usize> = (0..self.s.orders.len())
                        .filter(|&i| !self.s.orders[i].done)
                        .collect();
                    for i in idxs {
                        let o = &self.s.orders[i];
                        let listed = open.iter().any(|oo| is_listed(o, oo));
                        if listed && !o.uncertain {
                            continue;
                        }
                        out.extend(self.resolve(now_ms, i).await);
                    }
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "openOrders failed"),
            }
        }
        // Cancels that did not go through (transport error, or the order
        // turned out to be still open): retry.
        let retry: Vec<u64> = self
            .active()
            .filter(|o| o.cancel_requested && !o.uncertain)
            .map(|o| o.tag)
            .collect();
        for tag in retry {
            tracing::info!(tag, "retrying cancel");
            out.extend(self.cancel(now_ms, tag).await);
        }
        // Funding payments on the coin.
        match self.api.funding_since(self.s.last_funding_ms + 1).await {
            Ok(v) => {
                for (t, usd) in v {
                    if t > self.s.last_funding_ms {
                        self.s.last_funding_ms = t;
                        out.push(ExecEvent::Funding { usd, time_ms: t });
                    }
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "userFunding failed"),
        }
        self.prune(now_ms);
        out
    }

    fn available_margin_usd(&self) -> Option<f64> {
        self.available_margin
    }

    fn venue_position(&self) -> Option<f64> {
        self.venue.map(|v| v.szi)
    }

    fn save_state(&self) -> Value {
        serde_json::to_value(&self.s).unwrap_or(Value::Null)
    }

    async fn shutdown(&mut self, now_ms: u64, market: &MarketState) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        if self.p.live.cancel_on_shutdown {
            let tags: Vec<u64> = self.active().map(|o| o.tag).collect();
            for tag in tags {
                out.extend(self.cancel(now_ms, tag).await);
            }
        }
        if self.p.live.flatten_on_shutdown {
            if let Err(e) = self.refresh_account().await {
                out.push(ExecEvent::Alert(format!(
                    "shutdown flatten: cannot read position ({e:#})"
                )));
                return out;
            }
            let Some(v) = self.venue.filter(|v| v.szi.abs() > self.eps()) else {
                return out;
            };
            let is_buy = v.szi < 0.0;
            let slip = from_bps(self.p.taker_max_slippage_bps);
            let raw = if is_buy {
                market.best_ask.map(|l| l.px * (1.0 + slip))
            } else {
                market.best_bid.map(|l| l.px * (1.0 - slip))
            };
            let Some(px) = raw
                .and_then(|r| round_price(r, self.p.sz_decimals, RoundDir::passive(is_buy)).ok())
            else {
                out.push(ExecEvent::Alert(
                    "shutdown flatten: no book, position left open".into(),
                ));
                return out;
            };
            // Tag outside the strategy's range: a shutdown-only order.
            let tag = u64::MAX - now_ms % 1_000_000;
            out.extend(
                self.place(
                    now_ms,
                    OrderIntent {
                        tag,
                        is_buy,
                        px,
                        sz: v.szi.abs(),
                        kind: OrderKind::Ioc,
                        reduce_only: true,
                        purpose: crate::strategy::signal::Purpose::Exit,
                    },
                )
                .await,
            );
        }
        out
    }
}

#[cfg(test)]
mod tests;
