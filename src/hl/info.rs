//! Info REST client (`POST /info`): meta (dex), perpDexs, metaAndAssetCtxs
//! (dex), l2Book, clearinghouseState (dex), openOrders (dex), userFills,
//! candleSnapshot, fundingHistory.
//!
//! Rate limiting: a token bucket in *weight* units, mirroring the official
//! per-IP budget (1200 weight / minute, "Rate limits and user limits" docs):
//! `l2Book` / `clearinghouseState` weigh 2, other info requests 20, plus
//! +1 per 20 items returned for `userFills` / `fundingHistory` and +1 per 60
//! items for `candleSnapshot` (charged after the response, the bucket may go
//! into debt). Retries with exponential backoff on network errors, HTTP 429
//! and 5xx; other 4xx fail immediately.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::types::{
    Candle, ClearinghouseState, Fill, FundingHistoryEntry, L2Book, Meta, MetaAndAssetCtxs,
    OpenOrder, PerpDexs,
};

/// Official per-IP REST budget (weight per minute).
pub const HL_WEIGHT_PER_MIN: f64 = 1200.0;

/// Request weight per the official docs.
pub fn base_weight(request_type: &str) -> f64 {
    match request_type {
        "l2Book"
        | "allMids"
        | "clearinghouseState"
        | "orderStatus"
        | "spotClearinghouseState"
        | "exchangeStatus" => 2.0,
        "userRole" => 60.0,
        _ => 20.0,
    }
}

/// Additional weight charged after the response, from the number of items.
pub fn extra_weight(request_type: &str, items: usize) -> f64 {
    match request_type {
        "recentTrades"
        | "historicalOrders"
        | "userFills"
        | "userFillsByTime"
        | "fundingHistory"
        | "userFunding"
        | "nonUserFundingUpdates" => (items / 20) as f64,
        "candleSnapshot" => (items / 60) as f64,
        _ => 0.0,
    }
}

/// Async token bucket (weight units). `acquire` waits until enough tokens are
/// available; `charge` debits without waiting (may go negative).
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    state: Mutex<BucketState>,
}

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        assert!(capacity > 0.0 && refill_per_sec > 0.0);
        Self {
            capacity,
            refill_per_sec,
            state: Mutex::new(BucketState {
                tokens: capacity,
                last: Instant::now(),
            }),
        }
    }

    fn refill(&self, s: &mut BucketState) {
        let now = Instant::now();
        let dt = now.duration_since(s.last).as_secs_f64();
        s.tokens = (s.tokens + dt * self.refill_per_sec).min(self.capacity);
        s.last = now;
    }

    /// Wait until `weight` tokens are available and take them. A weight above
    /// the capacity is clamped to the capacity (it could never be served).
    pub async fn acquire(&self, weight: f64) {
        let w = weight.min(self.capacity);
        loop {
            let wait = {
                let mut s = self.state.lock().await;
                self.refill(&mut s);
                if s.tokens >= w {
                    s.tokens -= w;
                    return;
                }
                Duration::from_secs_f64((w - s.tokens) / self.refill_per_sec)
            };
            tracing::debug!(
                wait_ms = wait.as_millis() as u64,
                weight = w,
                "rate limit wait"
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// Debit `weight` immediately (post-response extra weight).
    pub async fn charge(&self, weight: f64) {
        if weight <= 0.0 {
            return;
        }
        let mut s = self.state.lock().await;
        self.refill(&mut s);
        s.tokens -= weight;
    }

    pub async fn available(&self) -> f64 {
        let mut s = self.state.lock().await;
        self.refill(&mut s);
        s.tokens
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_backoff: Duration::from_millis(300),
            max_backoff: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InfoError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("decode {request}: {source}; body starts with: {snippet}")]
    Decode {
        request: String,
        snippet: String,
        source: serde_json::Error,
    },
}

impl InfoError {
    fn retryable(&self) -> bool {
        match self {
            InfoError::Http { status, .. } => *status == 429 || *status >= 500,
            InfoError::Transport(_) => true,
            InfoError::Decode { .. } => false,
        }
    }
}

/// Info endpoint client. Cheap to clone (shared HTTP pool and bucket).
#[derive(Debug, Clone)]
pub struct InfoClient {
    http: reqwest::Client,
    url: String,
    bucket: Arc<TokenBucket>,
    retry: RetryPolicy,
}

impl InfoClient {
    pub fn new(
        info_url: impl Into<String>,
        timeout: Duration,
        bucket: Arc<TokenBucket>,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("antbot/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            url: info_url.into(),
            bucket,
            retry: RetryPolicy::default(),
        })
    }

    /// Build from the config (timeouts + bucket) for the given info URL.
    pub fn from_config(cfg: &crate::config::Config, info_url: &str) -> anyhow::Result<Self> {
        let bucket = Arc::new(TokenBucket::new(
            cfg.timing.rest_bucket_capacity,
            cfg.timing.rest_refill_per_sec,
        ));
        Self::new(
            info_url,
            Duration::from_secs(cfg.network.http_timeout_secs),
            bucket,
        )
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn bucket(&self) -> &Arc<TokenBucket> {
        &self.bucket
    }

    async fn post_once(&self, body: &Value) -> Result<Value, InfoError> {
        let resp = self.http.post(&self.url).json(body).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(InfoError::Http {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).into_owned(),
            });
        }
        serde_json::from_slice(&bytes).map_err(|e| InfoError::Decode {
            request: body["type"].as_str().unwrap_or("?").to_string(),
            snippet: String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned(),
            source: e,
        })
    }

    /// Rate-limited, retried raw request. Returns the JSON value.
    pub async fn post_raw(&self, body: Value) -> anyhow::Result<Value> {
        let rtype = body["type"].as_str().unwrap_or("").to_string();
        let mut backoff = self.retry.initial_backoff;
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.bucket.acquire(base_weight(&rtype)).await;
            match self.post_once(&body).await {
                Ok(v) => {
                    if let Some(n) = v.as_array().map(|a| a.len()) {
                        self.bucket.charge(extra_weight(&rtype, n)).await;
                    }
                    return Ok(v);
                }
                Err(e) if e.retryable() && attempt < self.retry.max_attempts => {
                    tracing::warn!(request = %rtype, attempt, error = %e, "info request failed, retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.retry.max_backoff);
                }
                Err(e) => {
                    return Err(anyhow!(e).context(format!(
                        "info request {rtype} failed after {attempt} attempt(s)"
                    )))
                }
            }
        }
    }

    /// Typed request.
    pub async fn post<T: DeserializeOwned>(&self, body: Value) -> anyhow::Result<T> {
        let rtype = body["type"].as_str().unwrap_or("").to_string();
        let v = self.post_raw(body).await?;
        serde_json::from_value(v).with_context(|| format!("decoding {rtype} response"))
    }

    /// `meta` for a perp dex (`""` = first/original dex).
    pub async fn meta(&self, dex: &str) -> anyhow::Result<Meta> {
        self.post(json!({"type": "meta", "dex": dex})).await
    }

    pub async fn perp_dexs(&self) -> anyhow::Result<PerpDexs> {
        self.post(json!({"type": "perpDexs"})).await
    }

    pub async fn meta_and_asset_ctxs(&self, dex: &str) -> anyhow::Result<MetaAndAssetCtxs> {
        self.post(json!({"type": "metaAndAssetCtxs", "dex": dex}))
            .await
    }

    pub async fn l2_book(&self, coin: &str) -> anyhow::Result<L2Book> {
        self.post(json!({"type": "l2Book", "coin": coin})).await
    }

    pub async fn clearinghouse_state(
        &self,
        user: &str,
        dex: &str,
    ) -> anyhow::Result<ClearinghouseState> {
        self.post(json!({"type": "clearinghouseState", "user": user, "dex": dex}))
            .await
    }

    pub async fn open_orders(&self, user: &str, dex: &str) -> anyhow::Result<Vec<OpenOrder>> {
        self.post(json!({"type": "openOrders", "user": user, "dex": dex}))
            .await
    }

    /// Most recent fills (max 2000) across all dexs.
    pub async fn user_fills(&self, user: &str) -> anyhow::Result<Vec<Fill>> {
        self.post(json!({"type": "userFills", "user": user})).await
    }

    /// Candles, `interval` e.g. `"1m"`, `"1h"`; times in ms.
    pub async fn candle_snapshot(
        &self,
        coin: &str,
        interval: &str,
        start_ms: u64,
        end_ms: u64,
    ) -> anyhow::Result<Vec<Candle>> {
        self.post(json!({
            "type": "candleSnapshot",
            "req": {"coin": coin, "interval": interval, "startTime": start_ms, "endTime": end_ms}
        }))
        .await
    }

    /// Hourly funding / premium history (max 500 rows per call).
    pub async fn funding_history(
        &self,
        coin: &str,
        start_ms: u64,
        end_ms: Option<u64>,
    ) -> anyhow::Result<Vec<FundingHistoryEntry>> {
        let mut body = json!({"type": "fundingHistory", "coin": coin, "startTime": start_ms});
        if let Some(e) = end_ms {
            body["endTime"] = json!(e);
        }
        self.post(body).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_match_docs() {
        assert_eq!(base_weight("l2Book"), 2.0);
        assert_eq!(base_weight("clearinghouseState"), 2.0);
        assert_eq!(base_weight("metaAndAssetCtxs"), 20.0);
        assert_eq!(base_weight("openOrders"), 20.0);
        assert_eq!(extra_weight("userFills", 2000), 100.0);
        assert_eq!(extra_weight("fundingHistory", 19), 0.0);
        assert_eq!(extra_weight("candleSnapshot", 120), 2.0);
        assert_eq!(extra_weight("meta", 500), 0.0);
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_waits_for_refill() {
        let b = TokenBucket::new(40.0, 10.0);
        let t0 = Instant::now();
        b.acquire(20.0).await;
        b.acquire(20.0).await;
        assert!(t0.elapsed() < Duration::from_millis(1));
        // Empty: the next 20 needs 2s of refill.
        b.acquire(20.0).await;
        let el = t0.elapsed();
        assert!(
            el >= Duration::from_millis(1990) && el < Duration::from_millis(2100),
            "{el:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_debt_and_clamp() {
        let b = TokenBucket::new(10.0, 5.0);
        b.charge(15.0).await; // -5
        assert!((b.available().await + 5.0).abs() < 1e-9);
        let t0 = Instant::now();
        // weight 50 > capacity -> clamped to 10: needs 15 tokens = 3s.
        b.acquire(50.0).await;
        let el = t0.elapsed();
        assert!(
            el >= Duration::from_millis(2990) && el < Duration::from_millis(3100),
            "{el:?}"
        );
    }

    /// Sustained throughput never exceeds the refill rate (+ initial burst).
    #[tokio::test(start_paused = true)]
    async fn bucket_sustained_rate() {
        let b = TokenBucket::new(100.0, 10.0);
        let t0 = Instant::now();
        for _ in 0..30 {
            b.acquire(20.0).await; // 600 weight
        }
        // (600 - 100 burst) / 10 per s = 50s
        let el = t0.elapsed().as_secs_f64();
        assert!((49.9..51.0).contains(&el), "{el}");
    }
}
