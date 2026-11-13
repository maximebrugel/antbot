//! Execution engines (paper / live) and persisted state.
//!
//! The strategy ([`crate::strategy::signal`]) emits [`Command`]s and expects
//! [`OrderUpdate`]s back, following the executor contract documented there:
//! every `Place` gets exactly one terminal `Done`, fills may arrive before or
//! after it, `tag` identifies the order. A `Cancel` for an order the
//! executor does not know (never placed) or that already finished is
//! answered with a terminal `Done` as well (a duplicate is ignored by the
//! strategy), so the strategy can never wait forever. Both executors honour
//! it:
//!
//! * [`paper::PaperExecutor`]: live mainnet market data, simulated fills,
//!   never touches `/exchange` and never needs a key;
//! * [`live::LiveExecutor`]: the signing exchange client (testnet, or
//!   mainnet behind the guard), startup reconciliation, userFills /
//!   orderUpdates, periodic resync, cancel-on-shutdown.

pub mod live;
pub mod paper;
pub mod state;

use crate::hl::market::MarketState;
use crate::hl::types::WsTrade;
use crate::hl::ws::UserEvent;
use crate::strategy::signal::{Command, DoneReason, OrderUpdate};

/// Something the executor reports back to the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecEvent {
    /// Order feedback for the strategy.
    Order(OrderUpdate),
    /// Funding settled on the open position (> 0 received).
    Funding { usd: f64, time_ms: u64 },
    /// Operator-level problem (rate limit, resync, mismatch...): log + alert.
    Alert(String),
}

/// What an executor knows about one strategy order (used to heal a
/// strategy that waits for a `Done` that will never come: a command dropped
/// at shutdown, a lost event...).
#[derive(Debug, Clone, PartialEq)]
pub enum OrderView {
    /// Never seen (or long forgotten).
    Unknown,
    /// Live / pending; `cancel_requested` once a cancel was sent.
    Active { cancel_requested: bool },
    /// Finished: the terminal state already reported.
    Done {
        reason: DoneReason,
        filled_sz: Option<f64>,
    },
}

impl From<OrderUpdate> for ExecEvent {
    fn from(u: OrderUpdate) -> Self {
        ExecEvent::Order(u)
    }
}

/// Common interface of the paper and live executors. Static dispatch only
/// (the main loop is generic over it).
#[allow(async_fn_in_trait)]
pub trait Executor {
    /// "paper" / "live".
    fn kind(&self) -> &'static str;

    /// Execute strategy commands. Returns the feedback known right away
    /// (acks, immediate fills, rejects); the rest comes later through
    /// [`Executor::on_market`], [`Executor::on_trades`] or
    /// [`Executor::on_user_event`].
    async fn execute(
        &mut self,
        now_ms: u64,
        cmds: Vec<Command>,
        market: &MarketState,
    ) -> Vec<ExecEvent>;

    /// New market state (paper: book-cross fills and funding).
    fn on_market(&mut self, now_ms: u64, market: &MarketState) -> Vec<ExecEvent>;

    /// Public trades for the coin (paper: trade-through fills).
    fn on_trades(&mut self, now_ms: u64, trades: &[WsTrade]) -> Vec<ExecEvent>;

    /// userFills / orderUpdates from the websocket (live).
    async fn on_user_event(&mut self, now_ms: u64, ev: UserEvent) -> Vec<ExecEvent>;

    /// Periodic reconciliation with the exchange (live). `force` after a
    /// dropped user event or an uncertain request outcome.
    async fn reconcile(&mut self, now_ms: u64, force: bool) -> Vec<ExecEvent>;

    /// Margin available for a new position, if known.
    fn available_margin_usd(&self) -> Option<f64>;

    /// Signed position on the venue (simulated account in paper), if known.
    fn venue_position(&self) -> Option<f64>;

    /// State of the order with strategy tag `tag`.
    fn order_view(&self, tag: u64) -> OrderView;

    /// Executor state to persist (JSON).
    fn save_state(&self) -> serde_json::Value;

    /// Graceful shutdown: cancel open orders (and flatten if configured).
    async fn shutdown(&mut self, now_ms: u64, market: &MarketState) -> Vec<ExecEvent>;

    /// Orders currently open (resting or in flight) on the venue / in the
    /// simulation, for monitoring.
    fn open_order_count(&self) -> usize {
        0
    }

    /// Consecutive failed reconciliation account reads (0 = the last one
    /// worked; always 0 for executors without reconciliation).
    fn reconcile_failures(&self) -> u32 {
        0
    }
}
