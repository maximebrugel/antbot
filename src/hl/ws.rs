//! Websocket market / user feed.
//!
//! * subscriptions: `l2Book{coin}`, `activeAssetCtx{coin}` and, when a user
//!   address is given (live modes), `userFills{user}` / `orderUpdates{user}`;
//! * heartbeat: `{"method":"ping"}` every `ws_ping_secs` (< 60 s, the server
//!   closes connections idle for 60 s — "Timeouts and heartbeats" docs);
//! * stale detection: no market message for `ws_stale_secs` -> the
//!   connection is dropped and re-established;
//! * reconnect with exponential backoff (+ jitter), resubscribing everything;
//! * REST fallback: while the websocket is stale, `metaAndAssetCtxs{dex}` +
//!   `l2Book{coin}` are polled every `rest_fallback_poll_secs`.
//!
//! Output: a `watch` channel of [`MarketState`] and an `mpsc` channel of
//! [`UserEvent`]s.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use super::info::InfoClient;
use super::market::{now_ms, DataSource, MarketState};
use super::types::{Fill, OrderUpdate, WsMessage};

#[derive(Debug, Clone)]
pub struct FeedConfig {
    pub ws_url: String,
    pub dex: String,
    pub coin: String,
    /// User address for userFills / orderUpdates (live modes only). Must be the
    /// account address, not an agent wallet.
    pub user: Option<String>,
    pub ping_interval: Duration,
    pub stale_after: Duration,
    pub rest_poll_interval: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    pub connect_timeout: Duration,
    /// false = REST polling only (diagnostics).
    pub ws_enabled: bool,
}

impl FeedConfig {
    pub fn from_config(cfg: &crate::config::Config, ws_url: &str, user: Option<String>) -> Self {
        let t = &cfg.timing;
        Self {
            ws_url: ws_url.to_string(),
            dex: cfg.market.dex.clone(),
            coin: cfg.market.coin.clone(),
            user,
            ping_interval: Duration::from_secs(t.ws_ping_secs),
            stale_after: Duration::from_secs(t.ws_stale_secs),
            rest_poll_interval: Duration::from_secs(t.rest_fallback_poll_secs),
            backoff_initial: Duration::from_millis(t.ws_reconnect_initial_ms),
            backoff_max: Duration::from_millis(t.ws_reconnect_max_ms),
            connect_timeout: Duration::from_secs(cfg.network.http_timeout_secs),
            ws_enabled: true,
        }
    }

    /// Subscription payloads sent on every (re)connection.
    pub fn subscriptions(&self) -> Vec<serde_json::Value> {
        let mut subs = vec![
            json!({"type": "l2Book", "coin": self.coin}),
            json!({"type": "activeAssetCtx", "coin": self.coin}),
        ];
        if let Some(u) = &self.user {
            subs.push(json!({"type": "userFills", "user": u}));
            subs.push(json!({"type": "orderUpdates", "user": u}));
        }
        subs.into_iter()
            .map(|s| json!({"method": "subscribe", "subscription": s}))
            .collect()
    }
}

/// User-level events (live modes).
#[derive(Debug, Clone, PartialEq)]
pub enum UserEvent {
    /// `is_snapshot` = initial batch replayed on (re)subscription.
    Fills {
        is_snapshot: bool,
        fills: Vec<Fill>,
    },
    OrderUpdates(Vec<OrderUpdate>),
}

/// Running feed. Drop or call [`MarketFeed::shutdown`] to stop it.
pub struct MarketFeed {
    pub state: watch::Receiver<MarketState>,
    pub user_events: mpsc::Receiver<UserEvent>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl MarketFeed {
    pub fn spawn(cfg: FeedConfig, info: InfoClient) -> Self {
        let (state_tx, state_rx) = watch::channel(MarketState::new(cfg.coin.clone()));
        let state_tx = Arc::new(state_tx);
        let (user_tx, user_rx) = mpsc::channel(1024);
        let (sd_tx, sd_rx) = watch::channel(false);
        let mut tasks = Vec::new();
        if cfg.ws_enabled {
            tasks.push(tokio::spawn(ws_task(
                cfg.clone(),
                state_tx.clone(),
                user_tx,
                sd_rx.clone(),
            )));
        }
        tasks.push(tokio::spawn(rest_fallback_task(cfg, info, state_tx, sd_rx)));
        Self {
            state: state_rx,
            user_events: user_rx,
            shutdown: sd_tx,
            tasks,
        }
    }

    /// Latest market state.
    pub fn snapshot(&self) -> MarketState {
        self.state.borrow().clone()
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        for t in std::mem::take(&mut self.tasks) {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
    }
}

impl Drop for MarketFeed {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// Exponential backoff with +/-20 % jitter.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            current: initial,
        }
    }
    pub fn reset(&mut self) {
        self.current = self.initial;
    }
    /// Next delay (jittered); doubles the base up to `max`.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.current;
        self.current = (self.current * 2).min(self.max);
        // cheap jitter source: sub-second clock nanos
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let f = 0.8 + 0.4 * (nanos % 1000) as f64 / 1000.0;
        base.mul_f64(f).min(self.max)
    }
}

enum ConnEnd {
    Shutdown,
}

async fn ws_task(
    cfg: FeedConfig,
    state: Arc<watch::Sender<MarketState>>,
    user_tx: mpsc::Sender<UserEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Backoff::new(cfg.backoff_initial, cfg.backoff_max);
    loop {
        if *shutdown.borrow() {
            break;
        }
        let mut got_data = false;
        let res = run_connection(&cfg, &state, &user_tx, &mut shutdown, &mut got_data).await;
        state.send_modify(|s| s.ws_connected = false);
        match res {
            Ok(ConnEnd::Shutdown) => break,
            Err(e) => {
                if got_data {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                tracing::warn!(error = %format!("{e:#}"), retry_in_ms = delay.as_millis() as u64, "websocket disconnected");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown.changed() => break,
                }
            }
        }
    }
    tracing::debug!("websocket task stopped");
}

async fn run_connection(
    cfg: &FeedConfig,
    state: &watch::Sender<MarketState>,
    user_tx: &mpsc::Sender<UserEvent>,
    shutdown: &mut watch::Receiver<bool>,
    got_data: &mut bool,
) -> anyhow::Result<ConnEnd> {
    let (ws, _) = tokio::time::timeout(
        cfg.connect_timeout,
        tokio_tungstenite::connect_async(cfg.ws_url.as_str()),
    )
    .await
    .map_err(|_| anyhow!("connect timeout"))?
    .with_context(|| format!("connecting to {}", cfg.ws_url))?;
    let (mut sink, mut stream) = ws.split();
    for sub in cfg.subscriptions() {
        sink.send(Message::text(sub.to_string())).await?;
    }
    let connected_at = now_ms();
    state.send_modify(|s| {
        s.ws_connected = true;
        s.ws_connects += 1;
    });
    tracing::info!(url = %cfg.ws_url, coin = %cfg.coin, user = cfg.user.is_some(), "websocket connected");

    let mut ping = tokio::time::interval(cfg.ping_interval);
    ping.tick().await; // first tick is immediate
    let mut check = tokio::time::interval(Duration::from_millis(500));
    let stale_ms = cfg.stale_after.as_millis() as u64;
    let mut last_market_ms = connected_at;

    loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    None => bail!("stream ended"),
                    Some(m) => m.context("websocket read")?,
                };
                match msg {
                    Message::Text(t) => {
                        let recv = now_ms();
                        if handle_text(cfg, t.as_str(), recv, state, user_tx).await? {
                            last_market_ms = recv;
                            *got_data = true;
                        }
                    }
                    Message::Close(f) => bail!("closed by server: {f:?}"),
                    // Ping frames are answered by tungstenite on the next write.
                    _ => {}
                }
            }
            _ = ping.tick() => {
                sink.send(Message::text(r#"{"method":"ping"}"#)).await.context("sending ping")?;
            }
            _ = check.tick() => {
                let age = now_ms().saturating_sub(last_market_ms);
                if age > stale_ms {
                    bail!("stale: no market data for {age} ms");
                }
            }
            _ = shutdown.changed() => {
                let _ = sink.send(Message::Close(None)).await;
                return Ok(ConnEnd::Shutdown);
            }
        }
    }
}

/// Handle one text frame. Returns true if it carried market data for our coin.
async fn handle_text(
    cfg: &FeedConfig,
    text: &str,
    recv_ms: u64,
    state: &watch::Sender<MarketState>,
    user_tx: &mpsc::Sender<UserEvent>,
) -> anyhow::Result<bool> {
    let msg = match WsMessage::parse(text) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, snippet = %&text[..text.len().min(200)], "unparseable ws message");
            return Ok(false);
        }
    };
    match msg {
        WsMessage::Book(b) => {
            if b.coin == cfg.coin {
                state.send_modify(|s| {
                    s.apply_book(&b, recv_ms, DataSource::Ws);
                });
                return Ok(true);
            }
        }
        WsMessage::AssetCtx(c) => {
            if c.coin == cfg.coin {
                state.send_modify(|s| s.apply_ctx(&c.ctx, recv_ms, DataSource::Ws));
                return Ok(true);
            }
        }
        WsMessage::UserFills(f) => {
            let ev = UserEvent::Fills {
                is_snapshot: f.is_snapshot.unwrap_or(false),
                fills: f.fills,
            };
            if user_tx.try_send(ev).is_err() {
                tracing::error!("user event channel full/closed, dropping fills");
            }
        }
        WsMessage::OrderUpdates(u) => {
            if user_tx.try_send(UserEvent::OrderUpdates(u)).is_err() {
                tracing::error!("user event channel full/closed, dropping order updates");
            }
        }
        WsMessage::Error(e) => bail!("server error: {e}"),
        WsMessage::SubscriptionResponse(v) => {
            tracing::debug!(subscription = %v, "subscribed");
        }
        WsMessage::Pong | WsMessage::Other(_) => {}
    }
    Ok(false)
}

/// Poll REST while the websocket is stale/disconnected.
async fn rest_fallback_task(
    cfg: FeedConfig,
    info: InfoClient,
    state: Arc<watch::Sender<MarketState>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let stale_ms = cfg.stale_after.as_millis() as u64;
    let mut tick = tokio::time::interval(cfg.rest_poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut was_polling = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => break,
        }
        if !state.borrow().ws_stale(now_ms(), stale_ms) {
            if was_polling {
                tracing::info!("websocket healthy again, REST fallback paused");
                was_polling = false;
            }
            continue;
        }
        if !was_polling {
            tracing::warn!("websocket stale, polling REST fallback");
            was_polling = true;
        }
        if let Err(e) = poll_rest_once(&cfg, &info, &state).await {
            tracing::warn!(error = %format!("{e:#}"), "REST fallback poll failed");
        }
    }
}

/// One REST poll: metaAndAssetCtxs{dex} + l2Book{coin} applied to the state.
pub async fn poll_rest_once(
    cfg: &FeedConfig,
    info: &InfoClient,
    state: &watch::Sender<MarketState>,
) -> anyhow::Result<()> {
    let (mac, book) =
        tokio::try_join!(info.meta_and_asset_ctxs(&cfg.dex), info.l2_book(&cfg.coin))?;
    let recv = now_ms();
    let (_, _, ctx) = mac
        .find(&cfg.coin)
        .ok_or_else(|| anyhow!("{} not in metaAndAssetCtxs({})", cfg.coin, cfg.dex))?;
    let ctx = ctx.clone();
    state.send_modify(|s| {
        s.apply_ctx(&ctx, recv, DataSource::Rest);
        s.apply_book(&book, recv, DataSource::Rest);
        s.rest_polls += 1;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = Backoff::new(Duration::from_millis(500), Duration::from_millis(3000));
        let d: Vec<u128> = (0..6).map(|_| b.next_delay().as_millis()).collect();
        let bases = [500u128, 1000, 2000, 3000, 3000, 3000];
        for (x, base) in d.iter().zip(bases) {
            assert!(
                *x >= base * 8 / 10 && *x <= (base * 12 / 10).min(3000),
                "{d:?}"
            );
        }
        b.reset();
        assert!(b.next_delay().as_millis() <= 600);
    }

    #[test]
    fn subscriptions_with_and_without_user() {
        let mut c = FeedConfig::from_config(&crate::config::Config::default(), "wss://x", None);
        let s = c.subscriptions();
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0],
            json!({"method":"subscribe","subscription":{"type":"l2Book","coin":"xyz:ANT"}})
        );
        assert_eq!(s[1]["subscription"]["type"], "activeAssetCtx");
        c.user = Some("0xabc".into());
        let s = c.subscriptions();
        assert_eq!(s.len(), 4);
        assert_eq!(
            s[2],
            json!({"method":"subscribe","subscription":{"type":"userFills","user":"0xabc"}})
        );
        assert_eq!(s[3]["subscription"]["type"], "orderUpdates");
    }
}
