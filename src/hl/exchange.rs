//! Exchange client (`POST /exchange`): order (ALO / IOC / reduce-only),
//! cancel, cancelByCloid, updateLeverage (isolated), scheduleCancel.
//!
//! Safety rules enforced by [`ExchangeClient::new`]:
//! * paper mode can never build a client (and has no exchange URL anyway);
//! * any URL that is not the testnet host or a loopback address is treated as
//!   mainnet, and a mainnet target needs a [`MainnetConfirmed`] token, which
//!   only `mode::confirm_mainnet` (flag + env) can produce;
//! * the mode and the URL must agree (testnet mode cannot hit a mainnet URL).
//!
//! Agent wallets: actions are signed with `HL_PRIVATE_KEY` (the agent / API
//! wallet). `HL_ACCOUNT_ADDRESS` is the main account, used for info queries
//! (positions, open orders, fills). It is NOT sent as `vaultAddress`: that
//! field is only for vaults / sub-accounts and stays `null`.
//!
//! Orders are never retried automatically: a transport error after the
//! request left is ambiguous, so the caller must reconcile by cloid.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use serde_json::Value;

use super::actions::{Action, CancelByCloidWire, CancelWire, Cloid, OrderWire, Tif};
use super::assets::AssetSpec;
use super::info::TokenBucket;
use super::rounding::{
    is_valid_perp_price, is_valid_size, round_price, round_size, RoundDir, RoundingError,
    MIN_ORDER_NOTIONAL_USD,
};
use super::signing::{sign_l1_action, Address, Signature, SigningError, Wallet};
use crate::config::{Endpoints, Mode};
use crate::mode::{Credentials, MainnetConfirmed};

pub const TESTNET_HOST: &str = "api.hyperliquid-testnet.xyz";

/// What network an exchange URL points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlNetwork {
    Testnet,
    /// 127.0.0.1 / localhost / ::1 (mock servers in tests).
    Loopback,
    /// Everything else, including the real mainnet host and unknown hosts.
    Mainnet,
}

/// Classify an exchange URL. Unknown hosts are conservatively mainnet.
pub fn classify_url(url: &str) -> UrlNetwork {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(""); // drop userinfo
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    }
    .to_ascii_lowercase();
    match host.as_str() {
        TESTNET_HOST => UrlNetwork::Testnet,
        "127.0.0.1" | "localhost" | "::1" => UrlNetwork::Loopback,
        _ => UrlNetwork::Mainnet,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("paper mode must never construct an exchange client")]
    PaperMode,
    #[error("no exchange URL for this mode")]
    NoExchangeUrl,
    #[error(
        "refusing mainnet exchange URL {url}: the mainnet guard (--mainnet AND \
         ANTBOT_CONFIRM_MAINNET=yes-i-understand) was not satisfied"
    )]
    MainnetNotAuthorized { url: String },
    #[error("mode {mode} does not match exchange URL {url} ({net:?})")]
    NetworkMismatch {
        mode: Mode,
        url: String,
        net: UrlNetwork,
    },
    #[error("credentials: {0}")]
    Credentials(#[from] SigningError),
    #[error("invalid order: {0}")]
    InvalidOrder(String),
    #[error(transparent)]
    Rounding(#[from] RoundingError),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("transport error (request outcome unknown, reconcile by cloid): {0}")]
    Transport(#[from] reqwest::Error),
    #[error("exchange rejected the action: {0}")]
    Api(String),
    #[error("unexpected exchange response: {0}")]
    Decode(String),
}

/// Below the 10 USD minimum order value, with a small epsilon: an exact
/// 10.00 USD notional computed in f64 may come out as 9.999999999999998.
pub fn below_min_notional(notional_usd: f64) -> bool {
    notional_usd < MIN_ORDER_NOTIONAL_USD - 1e-9
}

/// True for the exchange's rate-limit answers: per-IP (HTTP 429) or the
/// per-address action budget ("Too many cumulative requests sent ...").
pub fn is_rate_limit_msg(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("too many") || m.contains("rate limit")
}

impl ExchangeError {
    /// The action was refused because of a rate limit (back off, alert).
    pub fn is_rate_limited(&self) -> bool {
        match self {
            ExchangeError::Http { status, body } => *status == 429 || is_rate_limit_msg(body),
            ExchangeError::Api(m) => is_rate_limit_msg(m),
            _ => false,
        }
    }

    /// The request may or may not have reached the exchange.
    pub fn outcome_unknown(&self) -> bool {
        matches!(self, ExchangeError::Transport(_))
            || matches!(self, ExchangeError::Http { status, .. } if *status >= 500)
    }
}

/// Asset identity needed to build orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetRef {
    pub asset_id: u32,
    pub sz_decimals: u32,
}

impl From<&AssetSpec> for AssetRef {
    fn from(s: &AssetSpec) -> Self {
        Self {
            asset_id: s.asset_id,
            sz_decimals: s.sz_decimals,
        }
    }
}

/// A limit order ready for the wire (price/size already on tick/lot).
#[derive(Debug, Clone, PartialEq)]
pub struct OrderRequest {
    pub is_buy: bool,
    pub px: f64,
    pub sz: f64,
    pub tif: Tif,
    pub reduce_only: bool,
    pub cloid: Option<Cloid>,
}

impl OrderRequest {
    /// Build an order from raw values, rounding the price *passively* (bids
    /// down, asks up) and the size down. For ALO this keeps the order from
    /// crossing because of rounding; for an IOC taker fallback it keeps the
    /// limit inside the caller's slippage bound.
    pub fn rounded(
        asset: AssetRef,
        is_buy: bool,
        raw_px: f64,
        raw_sz: f64,
        tif: Tif,
        reduce_only: bool,
        cloid: Option<Cloid>,
    ) -> Result<Self, RoundingError> {
        Ok(Self {
            is_buy,
            px: round_price(raw_px, asset.sz_decimals, RoundDir::passive(is_buy))?,
            sz: round_size(raw_sz, asset.sz_decimals, RoundDir::Down)?,
            tif,
            reduce_only,
            cloid,
        })
    }

    /// Post-only maker order.
    pub fn alo(
        asset: AssetRef,
        is_buy: bool,
        px: f64,
        sz: f64,
        reduce_only: bool,
        cloid: Option<Cloid>,
    ) -> Result<Self, RoundingError> {
        Self::rounded(asset, is_buy, px, sz, Tif::Alo, reduce_only, cloid)
    }

    /// Immediate-or-cancel taker order with a limit price.
    pub fn ioc(
        asset: AssetRef,
        is_buy: bool,
        px: f64,
        sz: f64,
        reduce_only: bool,
        cloid: Option<Cloid>,
    ) -> Result<Self, RoundingError> {
        Self::rounded(asset, is_buy, px, sz, Tif::Ioc, reduce_only, cloid)
    }

    /// Check tick/lot/min-notional rules and build the wire order.
    pub fn to_wire(&self, asset: AssetRef) -> Result<OrderWire, ExchangeError> {
        if !is_valid_perp_price(self.px, asset.sz_decimals) {
            return Err(ExchangeError::InvalidOrder(format!(
                "price {} not on tick (5 sig figs, <= {} decimals)",
                self.px,
                6u32.saturating_sub(asset.sz_decimals)
            )));
        }
        if !is_valid_size(self.sz, asset.sz_decimals) {
            return Err(ExchangeError::InvalidOrder(format!(
                "size {} not on lot ({} decimals)",
                self.sz, asset.sz_decimals
            )));
        }
        if !self.reduce_only && below_min_notional(self.px * self.sz) {
            return Err(ExchangeError::InvalidOrder(format!(
                "notional {:.2} USD below the {MIN_ORDER_NOTIONAL_USD} USD minimum",
                self.px * self.sz
            )));
        }
        Ok(OrderWire::limit(
            asset.asset_id,
            self.is_buy,
            self.px,
            self.sz,
            self.reduce_only,
            self.tif,
            self.cloid,
        )?)
    }
}

/// Status of one order in an `order` response.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderStatus {
    Resting {
        oid: u64,
        cloid: Option<Cloid>,
    },
    Filled {
        oid: u64,
        total_sz: f64,
        avg_px: f64,
        cloid: Option<Cloid>,
    },
    Error(String),
    /// Anything else (e.g. "waitingForFill"), kept raw.
    Other(Value),
}

impl OrderStatus {
    pub fn oid(&self) -> Option<u64> {
        match self {
            OrderStatus::Resting { oid, .. } | OrderStatus::Filled { oid, .. } => Some(*oid),
            _ => None,
        }
    }
    /// ALO order rejected because it would have crossed the book.
    pub fn is_post_only_reject(&self) -> bool {
        matches!(self, OrderStatus::Error(m) if m.to_ascii_lowercase().contains("post only"))
    }
    /// IOC order that found nothing to match.
    pub fn is_ioc_no_match(&self) -> bool {
        matches!(self, OrderStatus::Error(m) if m.to_ascii_lowercase().contains("could not immediately match"))
    }
    /// Per-address / per-IP rate limit.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, OrderStatus::Error(m) if is_rate_limit_msg(m))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CancelStatus {
    Success,
    Error(String),
    Other(Value),
}

/// The signed JSON body posted to `/exchange`.
#[derive(Debug, Clone, Serialize)]
pub struct SignedRequest {
    pub action: Action,
    pub nonce: u64,
    pub signature: Signature,
    #[serde(rename = "vaultAddress")]
    pub vault_address: Option<String>,
    #[serde(rename = "expiresAfter")]
    pub expires_after: Option<u64>,
}

/// Strictly increasing millisecond nonces (`max(now_ms, last + 1)`).
#[derive(Debug, Default)]
pub struct NonceGen {
    last: Mutex<u64>,
}

impl NonceGen {
    pub fn next_at(&self, now_ms: u64) -> u64 {
        let mut last = self.last.lock().expect("nonce mutex");
        let n = now_ms.max(*last + 1);
        *last = n;
        n
    }
    pub fn next(&self) -> u64 {
        self.next_at(super::market::now_ms())
    }
}

/// Signing `/exchange` client. Not `Clone` on purpose: one per process so the
/// nonce generator stays monotonic.
pub struct ExchangeClient {
    http: reqwest::Client,
    url: String,
    is_mainnet: bool,
    wallet: Wallet,
    account: Address,
    expires_after_window_ms: Option<u64>,
    nonces: NonceGen,
    bucket: Option<Arc<TokenBucket>>,
}

impl std::fmt::Debug for ExchangeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExchangeClient")
            .field("url", &self.url)
            .field("is_mainnet", &self.is_mainnet)
            .field("signer", &self.wallet.address())
            .field("account", &self.account)
            .finish()
    }
}

impl ExchangeClient {
    /// Build a client. See the module docs for the guard rules.
    pub fn new(
        mode: Mode,
        endpoints: &Endpoints,
        creds: &Credentials,
        mainnet: Option<MainnetConfirmed>,
        timeout: Duration,
    ) -> Result<Self, ExchangeError> {
        let (url, is_mainnet) = Self::check_guard(mode, endpoints, mainnet)?;
        let wallet = Wallet::from_hex(&creds.private_key)?;
        let account = match &creds.account_address {
            Some(a) => a.parse::<Address>()?,
            None => wallet.address(),
        };
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("antbot/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(ExchangeError::Transport)?;
        Ok(Self {
            http,
            url,
            is_mainnet,
            wallet,
            account,
            expires_after_window_ms: None,
            nonces: NonceGen::default(),
            bucket: None,
        })
    }

    /// The guard alone (no key needed): returns the URL and whether actions
    /// are signed for mainnet (phantom agent source "a").
    pub fn check_guard(
        mode: Mode,
        endpoints: &Endpoints,
        mainnet: Option<MainnetConfirmed>,
    ) -> Result<(String, bool), ExchangeError> {
        if mode == Mode::Paper {
            return Err(ExchangeError::PaperMode);
        }
        let url = endpoints
            .exchange_url
            .clone()
            .ok_or(ExchangeError::NoExchangeUrl)?;
        let net = classify_url(&url);
        let wants_mainnet = mode == Mode::Mainnet || endpoints.is_mainnet;
        if (wants_mainnet || net == UrlNetwork::Mainnet) && mainnet.is_none() {
            return Err(ExchangeError::MainnetNotAuthorized { url });
        }
        let consistent = match mode {
            Mode::Testnet => !endpoints.is_mainnet && net != UrlNetwork::Mainnet,
            Mode::Mainnet => endpoints.is_mainnet && net != UrlNetwork::Testnet,
            Mode::Paper => false,
        };
        if !consistent {
            return Err(ExchangeError::NetworkMismatch { mode, url, net });
        }
        Ok((url, mode == Mode::Mainnet))
    }

    /// Reject actions whose nonce is older than `window_ms` (`expiresAfter`).
    pub fn with_expires_after_window(mut self, window_ms: Option<u64>) -> Self {
        self.expires_after_window_ms = window_ms;
        self
    }

    /// Share the REST token bucket with the info client (IP weight budget).
    pub fn with_bucket(mut self, bucket: Arc<TokenBucket>) -> Self {
        self.bucket = Some(bucket);
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn is_mainnet(&self) -> bool {
        self.is_mainnet
    }
    /// Address that signs (agent wallet or main wallet).
    pub fn signer_address(&self) -> Address {
        self.wallet.address()
    }
    /// Address to query state for (`HL_ACCOUNT_ADDRESS`, else the signer).
    pub fn account_address(&self) -> Address {
        self.account
    }
    pub fn is_agent(&self) -> bool {
        self.account != self.wallet.address()
    }

    /// Sign an action with an explicit nonce (pure; used by tests).
    pub fn sign_request(&self, action: Action, nonce: u64) -> Result<SignedRequest, ExchangeError> {
        let expires_after = self.expires_after_window_ms.map(|w| nonce + w);
        let signature = sign_l1_action(
            &self.wallet,
            &action,
            None,
            nonce,
            expires_after,
            self.is_mainnet,
        )?;
        Ok(SignedRequest {
            action,
            nonce,
            signature,
            vault_address: None,
            expires_after,
        })
    }

    /// Sign with a fresh nonce and POST. Never retried.
    pub async fn post_action(&self, action: Action) -> Result<Value, ExchangeError> {
        let weight = 1.0 + (action.batch_len() / 40) as f64;
        // Wait for the rate budget *before* taking the nonce, so a drained
        // bucket cannot eat the `expiresAfter` window or age the nonce.
        if let Some(b) = &self.bucket {
            b.acquire(weight).await;
        }
        let req = self.sign_request(action, self.nonces.next())?;
        tracing::debug!(
            action = req.action.type_name(),
            nonce = req.nonce,
            "posting exchange action"
        );
        let resp = self.http.post(&self.url).json(&req).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(ExchangeError::Http {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes[..bytes.len().min(500)]).into_owned(),
            });
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| {
            ExchangeError::Decode(format!(
                "{e}: {}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(300)])
            ))
        })?;
        check_ok(v)
    }

    /// Place one order.
    pub async fn order(
        &self,
        asset: AssetRef,
        req: &OrderRequest,
    ) -> Result<OrderStatus, ExchangeError> {
        let wire = req.to_wire(asset)?;
        let v = self.post_action(Action::orders(vec![wire])).await?;
        let mut st = parse_order_statuses(&v)?;
        if st.len() != 1 {
            return Err(ExchangeError::Decode(format!(
                "expected 1 order status, got {}",
                st.len()
            )));
        }
        Ok(st.remove(0))
    }

    /// Cancel by exchange order id.
    pub async fn cancel(&self, asset: AssetRef, oid: u64) -> Result<CancelStatus, ExchangeError> {
        let action = Action::Cancel {
            cancels: vec![CancelWire {
                a: asset.asset_id,
                o: oid,
            }],
        };
        single_cancel(self.post_action(action).await?)
    }

    /// Cancel by client order id.
    pub async fn cancel_by_cloid(
        &self,
        asset: AssetRef,
        cloid: Cloid,
    ) -> Result<CancelStatus, ExchangeError> {
        let action = Action::CancelByCloid {
            cancels: vec![CancelByCloidWire {
                asset: asset.asset_id,
                cloid,
            }],
        };
        single_cancel(self.post_action(action).await?)
    }

    /// Set leverage. The bot only uses isolated margin (`is_cross = false`).
    pub async fn update_leverage(
        &self,
        asset: AssetRef,
        leverage: u32,
        is_cross: bool,
    ) -> Result<(), ExchangeError> {
        if leverage == 0 {
            return Err(ExchangeError::InvalidOrder("leverage must be >= 1".into()));
        }
        self.post_action(Action::UpdateLeverage {
            asset: asset.asset_id,
            is_cross,
            leverage,
        })
        .await
        .map(|_| ())
    }

    /// `updateLeverage` with isolated margin.
    pub async fn update_leverage_isolated(
        &self,
        asset: AssetRef,
        leverage: u32,
    ) -> Result<(), ExchangeError> {
        self.update_leverage(asset, leverage, false).await
    }

    /// Dead man's switch: cancel all open orders at `time_ms` (None = unset).
    pub async fn schedule_cancel(&self, time_ms: Option<u64>) -> Result<(), ExchangeError> {
        self.post_action(Action::ScheduleCancel { time: time_ms })
            .await
            .map(|_| ())
    }
}

/// `{"status":"ok","response":...}` -> response; `"err"` -> `Api`.
fn check_ok(v: Value) -> Result<Value, ExchangeError> {
    match v.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(v),
        Some("err") => Err(ExchangeError::Api(match &v["response"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })),
        _ => Err(ExchangeError::Decode(v.to_string())),
    }
}

fn statuses(v: &Value) -> Result<&Vec<Value>, ExchangeError> {
    v.pointer("/response/data/statuses")
        .and_then(Value::as_array)
        .ok_or_else(|| ExchangeError::Decode(format!("no statuses in {v}")))
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn cloid_of(v: &Value) -> Option<Cloid> {
    v.get("cloid").and_then(Value::as_str)?.parse().ok()
}

/// Parse `response.data.statuses` of an `order` response.
pub fn parse_order_statuses(v: &Value) -> Result<Vec<OrderStatus>, ExchangeError> {
    statuses(v)?
        .iter()
        .map(|s| {
            if let Some(r) = s.get("resting") {
                let oid = r["oid"]
                    .as_u64()
                    .ok_or_else(|| ExchangeError::Decode(format!("resting without oid: {s}")))?;
                Ok(OrderStatus::Resting {
                    oid,
                    cloid: cloid_of(r),
                })
            } else if let Some(f) = s.get("filled") {
                let oid = f["oid"]
                    .as_u64()
                    .ok_or_else(|| ExchangeError::Decode(format!("filled without oid: {s}")))?;
                Ok(OrderStatus::Filled {
                    oid,
                    total_sz: num(&f["totalSz"]).unwrap_or(f64::NAN),
                    avg_px: num(&f["avgPx"]).unwrap_or(f64::NAN),
                    cloid: cloid_of(f),
                })
            } else if let Some(e) = s.get("error").and_then(Value::as_str) {
                Ok(OrderStatus::Error(e.to_string()))
            } else {
                Ok(OrderStatus::Other(s.clone()))
            }
        })
        .collect()
}

/// Parse `response.data.statuses` of a cancel response.
pub fn parse_cancel_statuses(v: &Value) -> Result<Vec<CancelStatus>, ExchangeError> {
    Ok(statuses(v)?
        .iter()
        .map(|s| match s {
            Value::String(x) if x == "success" => CancelStatus::Success,
            _ => match s.get("error").and_then(Value::as_str) {
                Some(e) => CancelStatus::Error(e.to_string()),
                None => CancelStatus::Other(s.clone()),
            },
        })
        .collect())
}

fn single_cancel(v: Value) -> Result<CancelStatus, ExchangeError> {
    let mut st = parse_cancel_statuses(&v)?;
    if st.len() != 1 {
        return Err(ExchangeError::Decode(format!(
            "expected 1 cancel status, got {}",
            st.len()
        )));
    }
    Ok(st.remove(0))
}

/// Client order id generator: high 64 bits = creation ms, low 64 bits = a
/// per-process random salt + counter. Unique across restarts in practice.
#[derive(Debug)]
pub struct CloidGen {
    salt: u64,
    counter: std::sync::atomic::AtomicU64,
}

impl Default for CloidGen {
    fn default() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        Self {
            salt: h.finish() & 0xffff_ffff_0000_0000,
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl CloidGen {
    pub fn next_at(&self, now_ms: u64) -> Cloid {
        let c = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Cloid(((now_ms as u128) << 64) | (self.salt | (c & 0xffff_ffff)) as u128)
    }
    pub fn next(&self) -> Cloid {
        self.next_at(super::market::now_ms())
    }
}

/// Convenience used by the live executor: build the client for a resolved
/// mode, sharing the info bucket.
pub fn build_for_mode(
    cfg: &crate::config::Config,
    mode: Mode,
    creds: &Credentials,
    mainnet: Option<MainnetConfirmed>,
    bucket: Arc<TokenBucket>,
) -> anyhow::Result<ExchangeClient> {
    let ep = cfg.endpoints(mode);
    let c = ExchangeClient::new(
        mode,
        &ep,
        creds,
        mainnet,
        Duration::from_secs(cfg.network.http_timeout_secs),
    )
    .context("building exchange client")?;
    Ok(c.with_bucket(bucket))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::hl::signing::recover_l1_signer;
    use crate::mode::{confirm_mainnet, CONFIRM_MAINNET_VALUE, ENV_CONFIRM_MAINNET};

    const KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";
    const ACCOUNT: &str = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";
    const ANT: AssetRef = AssetRef {
        asset_id: 110138,
        sz_decimals: 2,
    };

    fn creds(account: Option<&str>) -> Credentials {
        Credentials {
            private_key: KEY.into(),
            account_address: account.map(str::to_string),
        }
    }

    fn token() -> MainnetConfirmed {
        confirm_mainnet(true, |k: &str| {
            (k == ENV_CONFIRM_MAINNET).then(|| CONFIRM_MAINNET_VALUE.to_string())
        })
        .unwrap()
    }

    fn mk(
        mode: Mode,
        ep: &Endpoints,
        tok: Option<MainnetConfirmed>,
    ) -> Result<ExchangeClient, ExchangeError> {
        ExchangeClient::new(mode, ep, &creds(None), tok, Duration::from_secs(5))
    }

    #[test]
    fn url_classification() {
        assert_eq!(
            classify_url("https://api.hyperliquid-testnet.xyz/exchange"),
            UrlNetwork::Testnet
        );
        assert_eq!(
            classify_url("https://api.hyperliquid.xyz/exchange"),
            UrlNetwork::Mainnet
        );
        assert_eq!(
            classify_url("https://API.HYPERLIQUID-TESTNET.XYZ:443/exchange"),
            UrlNetwork::Testnet
        );
        assert_eq!(
            classify_url("http://127.0.0.1:8080/exchange"),
            UrlNetwork::Loopback
        );
        assert_eq!(
            classify_url("http://localhost/exchange"),
            UrlNetwork::Loopback
        );
        assert_eq!(
            classify_url("http://[::1]:9/exchange"),
            UrlNetwork::Loopback
        );
        // Unknown / tricky hosts are mainnet (conservative).
        assert_eq!(
            classify_url("https://api.hyperliquid-testnet.xyz.evil.com/exchange"),
            UrlNetwork::Mainnet
        );
        assert_eq!(
            classify_url("https://api.hyperliquid-testnet.xyz@api.hyperliquid.xyz/exchange"),
            UrlNetwork::Mainnet
        );
        assert_eq!(classify_url("https://example.com"), UrlNetwork::Mainnet);
    }

    #[test]
    fn paper_never_constructs() {
        let cfg = Config::default();
        let ep = cfg.endpoints(Mode::Paper);
        assert!(matches!(
            mk(Mode::Paper, &ep, None),
            Err(ExchangeError::PaperMode)
        ));
        // Even with a mainnet token and a forged exchange URL.
        let mut forged = ep.clone();
        forged.exchange_url = Some("https://api.hyperliquid-testnet.xyz/exchange".into());
        forged.is_mainnet = false;
        assert!(matches!(
            mk(Mode::Paper, &forged, Some(token())),
            Err(ExchangeError::PaperMode)
        ));
    }

    #[test]
    fn mainnet_refused_without_guard() {
        let cfg = Config::default();
        let ep = cfg.endpoints(Mode::Mainnet);
        assert!(matches!(
            mk(Mode::Mainnet, &ep, None),
            Err(ExchangeError::MainnetNotAuthorized { .. })
        ));
        // Testnet mode pointed at the mainnet URL: refused without token...
        let mut t = cfg.endpoints(Mode::Testnet);
        t.exchange_url = Some(cfg.network.mainnet_exchange_url.clone());
        assert!(matches!(
            mk(Mode::Testnet, &t, None),
            Err(ExchangeError::MainnetNotAuthorized { .. })
        ));
        // ...and still refused (mismatch) with one.
        assert!(matches!(
            mk(Mode::Testnet, &t, Some(token())),
            Err(ExchangeError::NetworkMismatch { .. })
        ));
        // Unknown host counts as mainnet.
        let mut u = cfg.endpoints(Mode::Testnet);
        u.exchange_url = Some("https://my-proxy.example/exchange".into());
        assert!(matches!(
            mk(Mode::Testnet, &u, None),
            Err(ExchangeError::MainnetNotAuthorized { .. })
        ));
        // Mainnet mode pointed at testnet: mismatch.
        let mut m = cfg.endpoints(Mode::Mainnet);
        m.exchange_url = Some(cfg.network.testnet_exchange_url.clone());
        assert!(matches!(
            mk(Mode::Mainnet, &m, Some(token())),
            Err(ExchangeError::NetworkMismatch { .. })
        ));
    }

    #[test]
    fn mainnet_allowed_only_with_token() {
        // Construction only: nothing is sent.
        let cfg = Config::default();
        let ep = cfg.endpoints(Mode::Mainnet);
        let c = mk(Mode::Mainnet, &ep, Some(token())).unwrap();
        assert!(c.is_mainnet());
        assert_eq!(c.url(), "https://api.hyperliquid.xyz/exchange");
    }

    #[test]
    fn testnet_ok_and_signs_for_testnet() {
        let cfg = Config::default();
        let c = mk(Mode::Testnet, &cfg.endpoints(Mode::Testnet), None).unwrap();
        assert!(!c.is_mainnet());
        assert!(!c.is_agent());
        assert_eq!(
            c.signer_address().to_hex(),
            "0x14791697260e4c9a71f18484c9f997b308e59325"
        );
    }

    #[test]
    fn agent_wallet_signs_with_key_and_queries_account() {
        let cfg = Config::default();
        let c = ExchangeClient::new(
            Mode::Testnet,
            &cfg.endpoints(Mode::Testnet),
            &creds(Some(ACCOUNT)),
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(c.is_agent());
        assert_eq!(c.account_address().to_hex(), ACCOUNT);
        let r = c
            .sign_request(
                Action::Cancel {
                    cancels: vec![CancelWire { a: 110138, o: 7 }],
                },
                1_762_975_000_123,
            )
            .unwrap();
        // Signed by the agent key, vaultAddress stays null.
        assert_eq!(r.vault_address, None);
        let signer =
            recover_l1_signer(&r.action, &r.signature, None, r.nonce, None, false).unwrap();
        assert_eq!(signer, c.signer_address());
        assert_ne!(signer, c.account_address());
    }

    #[test]
    fn signed_request_json_shape() {
        let cfg = Config::default();
        let c = mk(Mode::Testnet, &cfg.endpoints(Mode::Testnet), None)
            .unwrap()
            .with_expires_after_window(Some(30_000));
        let o = OrderRequest::alo(ANT, true, 104.5678, 23.9123, false, Some(Cloid(1))).unwrap();
        let r = c
            .sign_request(Action::orders(vec![o.to_wire(ANT).unwrap()]), 1000)
            .unwrap();
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["action"]["orders"][0]["p"], "104.56"); // bid rounded down
        assert_eq!(j["action"]["orders"][0]["s"], "23.91");
        assert_eq!(j["action"]["orders"][0]["t"]["limit"]["tif"], "Alo");
        assert_eq!(j["nonce"], 1000);
        assert_eq!(j["expiresAfter"], 31_000);
        assert!(j["vaultAddress"].is_null());
        assert!(j["signature"]["r"].as_str().unwrap().starts_with("0x"));
        let keys: Vec<&String> = j.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 5);
        let signer =
            recover_l1_signer(&r.action, &r.signature, None, 1000, Some(31_000), false).unwrap();
        assert_eq!(signer, c.signer_address());
    }

    #[test]
    fn order_request_rounding_and_validation() {
        let sell = OrderRequest::alo(ANT, false, 104.561, 10.0, true, None).unwrap();
        assert_eq!(sell.px, 104.57); // ask rounded up
        let ioc = OrderRequest::ioc(ANT, true, 105.1234, 1.239, true, None).unwrap();
        assert_eq!((ioc.px, ioc.sz), (105.12, 1.23));
        // Below 10 USD only allowed for reduce-only.
        let small = OrderRequest::alo(ANT, true, 104.5, 0.05, false, None).unwrap();
        assert!(matches!(
            small.to_wire(ANT),
            Err(ExchangeError::InvalidOrder(_))
        ));
        let small_ro = OrderRequest::alo(ANT, true, 104.5, 0.05, true, None).unwrap();
        assert!(small_ro.to_wire(ANT).is_ok());
        // Hand-built off-tick values are rejected.
        let bad = OrderRequest {
            is_buy: true,
            px: 104.567,
            sz: 1.0,
            tif: Tif::Alo,
            reduce_only: false,
            cloid: None,
        };
        assert!(bad.to_wire(ANT).is_err());
        let bad_sz = OrderRequest {
            px: 104.5,
            sz: 1.001,
            ..bad
        };
        assert!(bad_sz.to_wire(ANT).is_err());
    }

    #[test]
    fn nonce_strictly_increasing() {
        let g = NonceGen::default();
        assert_eq!(g.next_at(100), 100);
        assert_eq!(g.next_at(100), 101);
        assert_eq!(g.next_at(50), 102);
        assert_eq!(g.next_at(500), 500);
        assert!(g.next() > 500);
    }

    #[test]
    fn cloids_unique() {
        let g = CloidGen::default();
        let a = g.next_at(1);
        let b = g.next_at(1);
        assert_ne!(a, b);
        assert_eq!(a.to_raw().len(), 34);
        assert_eq!(a.0 >> 64, 1);
    }

    #[test]
    fn parse_responses() {
        let v: Value = serde_json::from_str(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
            {"resting":{"oid":77738308,"cloid":"0x00000000000000000000000000000001"}},
            {"filled":{"totalSz":"0.02","avgPx":"1891.4","oid":77747314}},
            {"error":"Post only order would have immediately matched, bbo was 104.5@104.6. asset=110138"},
            "waitingForFill"]}}}"#,
        )
        .unwrap();
        let st = parse_order_statuses(&check_ok(v).unwrap()).unwrap();
        assert_eq!(
            st[0],
            OrderStatus::Resting {
                oid: 77738308,
                cloid: Some(Cloid(1))
            }
        );
        assert_eq!(
            st[1],
            OrderStatus::Filled {
                oid: 77747314,
                total_sz: 0.02,
                avg_px: 1891.4,
                cloid: None
            }
        );
        assert!(st[2].is_post_only_reject());
        assert!(matches!(st[3], OrderStatus::Other(_)));
        assert_eq!(st[0].oid(), Some(77738308));

        let v: Value = serde_json::from_str(
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success",{"error":"Order was never placed, already canceled, or filled. asset=110138"}]}}}"#,
        )
        .unwrap();
        let st = parse_cancel_statuses(&v).unwrap();
        assert_eq!(st[0], CancelStatus::Success);
        assert!(matches!(&st[1], CancelStatus::Error(m) if m.contains("never placed")));

        let err: Value = serde_json::from_str(
            r#"{"status":"err","response":"User or API Wallet 0x1 does not exist."}"#,
        )
        .unwrap();
        assert!(
            matches!(check_ok(err), Err(ExchangeError::Api(m)) if m.contains("does not exist"))
        );
        let ok_default: Value =
            serde_json::from_str(r#"{"status":"ok","response":{"type":"default"}}"#).unwrap();
        assert!(check_ok(ok_default).is_ok());
    }

    #[test]
    fn min_notional_uses_epsilon() {
        let base = OrderRequest {
            is_buy: true,
            px: 33.33,
            sz: 0.3,
            tif: Tif::Alo,
            reduce_only: false,
            cloid: None,
        };
        // 9.999 USD: genuinely below the minimum.
        assert!(base.to_wire(ANT).is_err());
        let exact = OrderRequest {
            px: 12.5,
            sz: 0.8,
            ..base.clone()
        };
        assert!(exact.to_wire(ANT).is_ok(), "exactly 10 USD is allowed");
        // f64 round-off just under 10 must not count as below the minimum.
        assert!(!below_min_notional(9.999_999_999_999_998));
        assert!(below_min_notional(9.99));
    }

    #[test]
    fn rate_limit_classification() {
        assert!(is_rate_limit_msg(
            "Too many cumulative requests sent (10123 > 10000) for cumulative volume traded $0.00."
        ));
        assert!(ExchangeError::Api("Too many requests".into()).is_rate_limited());
        assert!(ExchangeError::Http {
            status: 429,
            body: String::new()
        }
        .is_rate_limited());
        assert!(!ExchangeError::Api("Insufficient margin".into()).is_rate_limited());
        assert!(OrderStatus::Error("Too many cumulative requests sent".into()).is_rate_limited());
        assert!(OrderStatus::Error(
            "Order could not immediately match against any resting orders. asset=110138".into()
        )
        .is_ioc_no_match());
        assert!(ExchangeError::Http {
            status: 502,
            body: String::new()
        }
        .outcome_unknown());
    }
}
