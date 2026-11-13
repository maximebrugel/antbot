//! Market state for the traded coin: book top, impact prices, oracle, mark,
//! funding and timestamps. Updated by the websocket feed (or the REST
//! fallback poller) and published on a `tokio::sync::watch` channel.

use serde::Serialize;

use super::types::{L2Book, Level, PerpAssetCtx};
use crate::config::PremiumSource;
use crate::strategy::premium::premium_from_bid_ask;

/// Where a piece of market data came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DataSource {
    #[default]
    None,
    Ws,
    Rest,
}

impl std::fmt::Display for DataSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DataSource::None => "none",
            DataSource::Ws => "ws",
            DataSource::Rest => "rest",
        })
    }
}

/// Wall clock in ms since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct MarketState {
    pub coin: String,
    // --- book (l2Book) ---
    pub best_bid: Option<Level>,
    pub best_ask: Option<Level>,
    /// Full depth of the last book (best first), used by the paper IOC
    /// depth walk.
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    /// Exchange timestamp of the book snapshot (ms).
    pub book_time_ms: Option<u64>,
    /// Local receive time of the book (ms).
    pub book_recv_ms: Option<u64>,
    pub book_source: DataSource,
    /// Websocket connection epoch (`ws_connects` at receive time) of a ws
    /// book. A ws book from a previous connection is not trusted.
    pub book_epoch: u64,
    // --- asset ctx (activeAssetCtx / metaAndAssetCtxs) ---
    pub oracle_px: Option<f64>,
    pub mark_px: Option<f64>,
    pub mid_px: Option<f64>,
    pub impact_bid_px: Option<f64>,
    pub impact_ask_px: Option<f64>,
    /// Exchange-computed premium (fraction), as published in the ctx.
    pub api_premium: Option<f64>,
    /// Current hourly funding rate (fraction).
    pub funding: Option<f64>,
    pub open_interest: Option<f64>,
    pub ctx_recv_ms: Option<u64>,
    pub ctx_source: DataSource,
    // --- connection ---
    pub ws_connected: bool,
    /// Local time of the last market message received over the websocket.
    pub last_ws_market_ms: Option<u64>,
    /// Number of (re)connections established so far.
    pub ws_connects: u64,
    /// Number of REST fallback polls applied.
    pub rest_polls: u64,
    /// User events (fills / order updates) the feed could not deliver
    /// (channel full or closed). Any increase forces a live resync.
    pub user_events_dropped: u64,
    /// Incremented on every update.
    pub seq: u64,
}

/// A websocket book older than this is stale even on a live connection
/// (l2Book is pushed only on changes, but never silent for this long).
pub const MAX_WS_BOOK_AGE_MS: u64 = 300_000;

impl MarketState {
    pub fn new(coin: impl Into<String>) -> Self {
        Self {
            coin: coin.into(),
            ..Default::default()
        }
    }

    /// Apply a book snapshot (ignored if for another coin or empty-sided book
    /// is returned as `None` sides).
    pub fn apply_book(&mut self, book: &L2Book, recv_ms: u64, source: DataSource) -> bool {
        if book.coin != self.coin {
            return false;
        }
        // Drop out-of-order snapshots (REST and WS can interleave).
        if let (Some(prev), DataSource::Rest) = (self.book_time_ms, source) {
            if book.time < prev {
                return false;
            }
        }
        self.best_bid = book.best_bid();
        self.best_ask = book.best_ask();
        self.bids = book.bids().to_vec();
        self.asks = book.asks().to_vec();
        self.book_time_ms = Some(book.time);
        self.book_recv_ms = Some(recv_ms);
        self.book_source = source;
        if source == DataSource::Ws {
            self.last_ws_market_ms = Some(recv_ms);
            self.book_epoch = self.ws_connects;
        }
        self.seq += 1;
        true
    }

    pub fn apply_ctx(&mut self, ctx: &PerpAssetCtx, recv_ms: u64, source: DataSource) {
        self.oracle_px = Some(ctx.oracle_px);
        self.mark_px = Some(ctx.mark_px);
        self.mid_px = ctx.mid_px;
        self.impact_bid_px = ctx.impact_pxs.map(|p| p.0);
        self.impact_ask_px = ctx.impact_pxs.map(|p| p.1);
        self.api_premium = ctx.premium;
        self.funding = Some(ctx.funding);
        self.open_interest = Some(ctx.open_interest);
        self.ctx_recv_ms = Some(recv_ms);
        self.ctx_source = source;
        if source == DataSource::Ws {
            self.last_ws_market_ms = Some(recv_ms);
        }
        self.seq += 1;
    }

    /// Apply a REST ctx fetched by a request that started at `req_start_ms`.
    /// Skipped if a websocket ctx arrived after the request started: the
    /// REST answer is then older than what we have (out-of-order guard).
    pub fn apply_rest_ctx(&mut self, ctx: &PerpAssetCtx, req_start_ms: u64, recv_ms: u64) -> bool {
        if self.ctx_source == DataSource::Ws && self.ctx_recv_ms.is_some_and(|t| t >= req_start_ms)
        {
            return false;
        }
        self.apply_ctx(ctx, recv_ms, DataSource::Rest);
        true
    }

    /// Mark the start of a new websocket connection: books from earlier
    /// connections stop counting as live.
    pub fn on_ws_connected(&mut self) {
        self.ws_connected = true;
        self.ws_connects += 1;
    }

    pub fn book_mid(&self) -> Option<f64> {
        Some(0.5 * (self.best_bid?.px + self.best_ask?.px))
    }

    pub fn impact_mid(&self) -> Option<f64> {
        Some(0.5 * (self.impact_bid_px? + self.impact_ask_px?))
    }

    /// Premium vs oracle (fraction) for the configured source.
    pub fn premium(&self, source: PremiumSource) -> Option<f64> {
        let oracle = self.oracle_px?;
        match source {
            PremiumSource::Impact => {
                premium_from_bid_ask(self.impact_bid_px?, self.impact_ask_px?, oracle)
            }
            PremiumSource::BookMid => {
                premium_from_bid_ask(self.best_bid?.px, self.best_ask?.px, oracle)
            }
        }
    }

    /// Spread of the book top in bps of the mid.
    pub fn spread_bps(&self) -> Option<f64> {
        let (b, a) = (self.best_bid?.px, self.best_ask?.px);
        Some((a - b) / (0.5 * (a + b)) * 1e4)
    }

    /// Age (ms) of the oldest of book / ctx; `None` if either is missing.
    pub fn data_age_ms(&self, now_ms: u64) -> Option<u64> {
        let b = self.book_recv_ms?;
        let c = self.ctx_recv_ms?;
        Some(now_ms.saturating_sub(b.min(c)))
    }

    /// True if the data needed by `source` is missing or older than `max_age_ms`.
    ///
    /// The ctx (oracle / impact prices) streams continuously (~1/s) and must
    /// be fresh. The websocket `l2Book` is only pushed when the book changes
    /// (gaps of 10+ s observed live on xyz:ANT), so a book received over the
    /// *current* websocket connection is valid up to [`MAX_WS_BOOK_AGE_MS`];
    /// a REST book (or one from an earlier connection) must be younger than
    /// `max_age_ms`.
    pub fn is_stale(&self, now_ms: u64, max_age_ms: u64, source: PremiumSource) -> bool {
        let fresh = |t: Option<u64>| t.is_some_and(|t| now_ms.saturating_sub(t) <= max_age_ms);
        let book_ok = self.best_bid.is_some()
            && self.best_ask.is_some()
            && self.book_fresh(now_ms, max_age_ms);
        match source {
            PremiumSource::Impact => !fresh(self.ctx_recv_ms),
            PremiumSource::BookMid => !fresh(self.ctx_recv_ms) || !book_ok,
        }
    }

    /// The book can be trusted for pricing orders (see [`Self::is_stale`]).
    pub fn book_fresh(&self, now_ms: u64, max_age_ms: u64) -> bool {
        let age = |t: Option<u64>| t.map(|t| now_ms.saturating_sub(t));
        let live_ws = self.book_source == DataSource::Ws
            && self.ws_connected
            && self.book_epoch == self.ws_connects
            && age(self.book_recv_ms).is_some_and(|a| a <= MAX_WS_BOOK_AGE_MS);
        live_ws || age(self.book_recv_ms).is_some_and(|a| a <= max_age_ms)
    }

    pub fn ctx_age_ms(&self, now_ms: u64) -> Option<u64> {
        self.ctx_recv_ms.map(|t| now_ms.saturating_sub(t))
    }

    pub fn book_age_ms(&self, now_ms: u64) -> Option<u64> {
        self.book_recv_ms.map(|t| now_ms.saturating_sub(t))
    }

    /// True if the websocket has not delivered market data for `max_age_ms`.
    pub fn ws_stale(&self, now_ms: u64, max_age_ms: u64) -> bool {
        !self.ws_connected
            || self
                .last_ws_market_ms
                .is_none_or(|t| now_ms.saturating_sub(t) > max_age_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(oracle: f64, ib: f64, ia: f64) -> PerpAssetCtx {
        PerpAssetCtx {
            funding: 0.0,
            open_interest: 1.0,
            prev_day_px: None,
            day_ntl_vlm: None,
            premium: None,
            oracle_px: oracle,
            mark_px: oracle,
            mid_px: None,
            impact_pxs: Some((ib, ia)),
            day_base_vlm: None,
        }
    }

    fn book(coin: &str, t: u64, bid: f64, ask: f64) -> L2Book {
        let l = |px| Level { px, sz: 1.0, n: 1 };
        L2Book {
            coin: coin.into(),
            time: t,
            levels: (vec![l(bid)], vec![l(ask)]),
        }
    }

    #[test]
    fn premium_sources() {
        let mut s = MarketState::new("xyz:ANT");
        assert_eq!(s.premium(PremiumSource::Impact), None);
        s.apply_ctx(&ctx(100.0, 100.4, 100.6), 1_000, DataSource::Ws);
        s.apply_book(&book("xyz:ANT", 5, 100.9, 101.1), 1_000, DataSource::Ws);
        assert!((s.premium(PremiumSource::Impact).unwrap() - 0.005).abs() < 1e-12);
        assert!((s.premium(PremiumSource::BookMid).unwrap() - 0.01).abs() < 1e-12);
        assert!((s.spread_bps().unwrap() - 0.2 / 101.0 * 1e4).abs() < 1e-9);
        assert_eq!(s.seq, 2);
    }

    #[test]
    fn ignores_other_coin_and_old_rest_books() {
        let mut s = MarketState::new("xyz:ANT");
        assert!(!s.apply_book(&book("BTC", 5, 1.0, 2.0), 0, DataSource::Ws));
        assert!(s.apply_book(&book("xyz:ANT", 10, 1.0, 2.0), 0, DataSource::Ws));
        assert!(!s.apply_book(&book("xyz:ANT", 9, 1.0, 2.0), 0, DataSource::Rest));
        assert!(s.apply_book(&book("xyz:ANT", 11, 1.0, 2.0), 0, DataSource::Rest));
        assert_eq!(s.book_source, DataSource::Rest);
    }

    #[test]
    fn staleness() {
        let mut s = MarketState::new("xyz:ANT");
        assert!(s.is_stale(0, 15_000, PremiumSource::Impact));
        assert!(s.ws_stale(0, 15_000));
        s.ws_connected = true;
        s.apply_book(&book("xyz:ANT", 1, 1.0, 2.0), 1_000, DataSource::Ws);
        s.apply_ctx(&ctx(1.5, 1.4, 1.6), 10_000, DataSource::Ws);
        assert!(!s.is_stale(20_000, 15_000, PremiumSource::Impact));
        // book is 19s old but streamed over a live websocket: still valid.
        assert!(!s.is_stale(20_000, 15_000, PremiumSource::BookMid));
        s.ws_connected = false;
        assert!(s.is_stale(20_000, 15_000, PremiumSource::BookMid));
        s.ws_connected = true;
        assert!(!s.ws_stale(20_000, 15_000));
        assert!(s.ws_stale(26_000, 15_000));
        assert!(s.is_stale(26_000, 15_000, PremiumSource::Impact));
        assert_eq!(s.data_age_ms(20_000), Some(19_000));
        // REST data does not refresh the websocket liveness.
        s.apply_ctx(&ctx(1.5, 1.4, 1.6), 30_000, DataSource::Rest);
        assert!(s.ws_stale(30_000, 15_000));
        assert!(!s.is_stale(30_000, 15_000, PremiumSource::Impact));
        // REST book: valid only while young.
        s.ws_connected = false;
        s.apply_book(&book("xyz:ANT", 2, 1.0, 2.0), 30_000, DataSource::Rest);
        assert!(!s.is_stale(31_000, 15_000, PremiumSource::BookMid));
        assert!(s.is_stale(46_000, 15_000, PremiumSource::BookMid));
        assert_eq!(s.ctx_age_ms(31_000), Some(1_000));
    }

    #[test]
    fn ws_book_from_previous_connection_is_not_live() {
        let mut s = MarketState::new("xyz:ANT");
        s.on_ws_connected();
        s.apply_book(&book("xyz:ANT", 1, 1.0, 2.0), 1_000, DataSource::Ws);
        s.apply_ctx(&ctx(1.5, 1.4, 1.6), 40_000, DataSource::Ws);
        assert_eq!(s.book_epoch, 1);
        assert!(!s.is_stale(40_000, 15_000, PremiumSource::BookMid));
        // Reconnect: subscriptions sent, but no new book yet.
        s.ws_connected = false;
        s.on_ws_connected();
        assert!(s.is_stale(40_000, 15_000, PremiumSource::BookMid));
        // The new connection delivers a book: live again.
        s.apply_book(&book("xyz:ANT", 2, 1.0, 2.0), 40_000, DataSource::Ws);
        assert!(!s.is_stale(41_000, 15_000, PremiumSource::BookMid));
        // Even on a live connection, a ws book older than 5 min is stale.
        s.apply_ctx(
            &ctx(1.5, 1.4, 1.6),
            40_000 + MAX_WS_BOOK_AGE_MS + 1,
            DataSource::Ws,
        );
        assert!(!s.book_fresh(40_000 + MAX_WS_BOOK_AGE_MS + 1, 15_000));
        assert_eq!(s.bids.len(), 1);
    }

    #[test]
    fn rest_ctx_does_not_overwrite_newer_ws_ctx() {
        let mut s = MarketState::new("xyz:ANT");
        s.apply_ctx(&ctx(1.0, 1.0, 1.0), 1_000, DataSource::Ws);
        // REST request started at 500, before the ws ctx: dropped.
        assert!(!s.apply_rest_ctx(&ctx(2.0, 2.0, 2.0), 500, 1_200));
        assert_eq!(s.oracle_px, Some(1.0));
        assert_eq!(s.ctx_recv_ms, Some(1_000));
        // Request started after the last ws ctx: applied.
        assert!(s.apply_rest_ctx(&ctx(3.0, 3.0, 3.0), 2_000, 2_100));
        assert_eq!(s.oracle_px, Some(3.0));
        assert_eq!(s.ctx_source, DataSource::Rest);
        // Successive REST answers are always applied.
        assert!(s.apply_rest_ctx(&ctx(4.0, 4.0, 4.0), 2_050, 2_300));
    }
}
