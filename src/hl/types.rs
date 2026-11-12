//! Wire types for the Hyperliquid info / websocket APIs.
//!
//! Hyperliquid encodes decimals as JSON strings (`"104.57"`); the
//! [`num`] helpers accept both strings and numbers. Structs never use
//! `deny_unknown_fields`: the API adds fields over time (e.g. `growthMode`,
//! `marginMode` for HIP-3) and the bot must keep running when it does.
//! Shapes were checked against live mainnet responses (fixtures under
//! `tests/fixtures/`).

use serde::{Deserialize, Serialize};

/// Lenient numeric deserializers: `"1.5"`, `1.5`, `null` / missing.
pub mod num {
    use serde::{de, Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrNum {
        S(String),
        N(f64),
    }

    fn parse<E: de::Error>(v: StrOrNum) -> Result<f64, E> {
        match v {
            StrOrNum::N(n) => Ok(n),
            StrOrNum::S(s) => s
                .trim()
                .parse::<f64>()
                .map_err(|e| E::custom(format!("invalid decimal {s:?}: {e}"))),
        }
    }

    pub fn f64<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        parse(StrOrNum::deserialize(d)?)
    }

    pub fn opt_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        match Option::<StrOrNum>::deserialize(d)? {
            None => Ok(None),
            Some(v) => parse(v).map(Some),
        }
    }

    pub fn opt_pair<'de, D: Deserializer<'de>>(d: D) -> Result<Option<(f64, f64)>, D::Error> {
        match Option::<Vec<StrOrNum>>::deserialize(d)? {
            None => Ok(None),
            Some(v) if v.len() == 2 => {
                let mut it = v.into_iter();
                let a = parse(it.next().expect("len 2"))?;
                let b = parse(it.next().expect("len 2"))?;
                Ok(Some((a, b)))
            }
            Some(v) => Err(de::Error::custom(format!(
                "expected a pair, got {} items",
                v.len()
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// perpDexs
// ---------------------------------------------------------------------------

/// One entry of the `perpDexs` response. Index 0 of the response is `null`
/// (the original/first perp dex); builder dexs follow in deployment order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpDex {
    pub name: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub deployer: Option<String>,
    #[serde(default)]
    pub oracle_updater: Option<String>,
    #[serde(default)]
    pub fee_recipient: Option<String>,
    /// `[coin, decimal-string]` pairs.
    #[serde(default)]
    pub asset_to_streaming_oi_cap: Vec<(String, String)>,
    #[serde(default)]
    pub asset_to_funding_multiplier: Vec<(String, String)>,
    #[serde(default)]
    pub asset_to_funding_interest_rate: Vec<(String, String)>,
    #[serde(default)]
    pub asset_to_funding_clamp: Vec<serde_json::Value>,
    #[serde(default)]
    pub sub_deployers: Vec<serde_json::Value>,
}

impl PerpDex {
    fn lookup(list: &[(String, String)], coin: &str) -> Option<f64> {
        list.iter()
            .find(|(c, _)| c == coin)
            .and_then(|(_, v)| v.parse::<f64>().ok())
    }
    pub fn funding_multiplier(&self, coin: &str) -> Option<f64> {
        Self::lookup(&self.asset_to_funding_multiplier, coin)
    }
    pub fn funding_interest_rate(&self, coin: &str) -> Option<f64> {
        Self::lookup(&self.asset_to_funding_interest_rate, coin)
    }
    pub fn streaming_oi_cap(&self, coin: &str) -> Option<f64> {
        Self::lookup(&self.asset_to_streaming_oi_cap, coin)
    }
    /// Raw funding clamp entry for `coin`, if any (format undocumented, kept raw).
    pub fn funding_clamp(&self, coin: &str) -> Option<serde_json::Value> {
        self.asset_to_funding_clamp.iter().find_map(|e| match e {
            serde_json::Value::Array(a) if a.first().and_then(|c| c.as_str()) == Some(coin) => {
                a.get(1).cloned()
            }
            _ => None,
        })
    }
}

/// `perpDexs` response: `[null, {xyz}, {flx}, ...]`.
pub type PerpDexs = Vec<Option<PerpDex>>;

// ---------------------------------------------------------------------------
// meta / metaAndAssetCtxs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetMeta {
    pub name: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    #[serde(default)]
    pub margin_table_id: Option<u32>,
    /// Absent for assets that allow cross margin.
    #[serde(default)]
    pub only_isolated: Option<bool>,
    /// `"noCross"` / `"strictIsolated"` / absent (cross allowed).
    #[serde(default)]
    pub margin_mode: Option<String>,
    /// HIP-3 growth mode (`"enabled"` => reduced fees).
    #[serde(default)]
    pub growth_mode: Option<String>,
    #[serde(default)]
    pub last_fee_scale_change_time: Option<String>,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub deployer_fee_scale: Option<f64>,
    #[serde(default)]
    pub is_delisted: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub universe: Vec<AssetMeta>,
    #[serde(default)]
    pub margin_tables: serde_json::Value,
    #[serde(default)]
    pub collateral_token: Option<u32>,
}

impl Meta {
    pub fn index_of(&self, coin: &str) -> Option<usize> {
        self.universe.iter().position(|a| a.name == coin)
    }
}

/// Perp asset context (REST `metaAndAssetCtxs[1][i]` and WS
/// `activeAssetCtx.ctx`). `premium` / `impactPxs` / `midPx` may be null.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpAssetCtx {
    #[serde(deserialize_with = "num::f64")]
    pub funding: f64,
    #[serde(deserialize_with = "num::f64")]
    pub open_interest: f64,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub prev_day_px: Option<f64>,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub day_ntl_vlm: Option<f64>,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub premium: Option<f64>,
    #[serde(deserialize_with = "num::f64")]
    pub oracle_px: f64,
    #[serde(deserialize_with = "num::f64")]
    pub mark_px: f64,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub mid_px: Option<f64>,
    /// `(impact_bid_px, impact_ask_px)`.
    #[serde(default, deserialize_with = "num::opt_pair")]
    pub impact_pxs: Option<(f64, f64)>,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub day_base_vlm: Option<f64>,
}

/// `metaAndAssetCtxs` response: `[meta, [ctx, ...]]`, same order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaAndAssetCtxs(pub Meta, pub Vec<PerpAssetCtx>);

impl MetaAndAssetCtxs {
    pub fn find(&self, coin: &str) -> Option<(usize, &AssetMeta, &PerpAssetCtx)> {
        let i = self.0.index_of(coin)?;
        Some((i, &self.0.universe[i], self.1.get(i)?))
    }
}

// ---------------------------------------------------------------------------
// l2Book
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Level {
    #[serde(deserialize_with = "num::f64")]
    pub px: f64,
    #[serde(deserialize_with = "num::f64")]
    pub sz: f64,
    pub n: u32,
}

/// `l2Book` (REST) / `WsBook` (WS). `levels[0]` = bids (desc), `levels[1]` = asks (asc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct L2Book {
    pub coin: String,
    pub time: u64,
    pub levels: (Vec<Level>, Vec<Level>),
}

impl L2Book {
    pub fn bids(&self) -> &[Level] {
        &self.levels.0
    }
    pub fn asks(&self) -> &[Level] {
        &self.levels.1
    }
    pub fn best_bid(&self) -> Option<Level> {
        self.levels.0.first().copied()
    }
    pub fn best_ask(&self) -> Option<Level> {
        self.levels.1.first().copied()
    }
}

// ---------------------------------------------------------------------------
// User state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    #[serde(deserialize_with = "num::f64")]
    pub account_value: f64,
    #[serde(deserialize_with = "num::f64")]
    pub total_ntl_pos: f64,
    #[serde(deserialize_with = "num::f64")]
    pub total_raw_usd: f64,
    #[serde(deserialize_with = "num::f64")]
    pub total_margin_used: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Leverage {
    /// `"cross"` | `"isolated"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub value: u32,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub raw_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub coin: String,
    /// Signed size (> 0 long, < 0 short).
    #[serde(deserialize_with = "num::f64")]
    pub szi: f64,
    pub leverage: Leverage,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub entry_px: Option<f64>,
    #[serde(deserialize_with = "num::f64")]
    pub position_value: f64,
    #[serde(deserialize_with = "num::f64")]
    pub unrealized_pnl: f64,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub liquidation_px: Option<f64>,
    #[serde(deserialize_with = "num::f64")]
    pub margin_used: f64,
    #[serde(default)]
    pub max_leverage: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetPosition {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    pub position: Position,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
    pub margin_summary: MarginSummary,
    #[serde(default)]
    pub cross_margin_summary: Option<MarginSummary>,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub withdrawable: Option<f64>,
    pub asset_positions: Vec<AssetPosition>,
    pub time: u64,
}

impl ClearinghouseState {
    pub fn position(&self, coin: &str) -> Option<&Position> {
        self.asset_positions
            .iter()
            .map(|p| &p.position)
            .find(|p| p.coin == coin && p.szi != 0.0)
    }
}

/// `openOrders` entry and WS `WsBasicOrder`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrder {
    pub coin: String,
    /// `"B"` (bid/buy) | `"A"` (ask/sell).
    pub side: String,
    #[serde(deserialize_with = "num::f64")]
    pub limit_px: f64,
    #[serde(deserialize_with = "num::f64")]
    pub sz: f64,
    pub oid: u64,
    pub timestamp: u64,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub orig_sz: Option<f64>,
    #[serde(default)]
    pub cloid: Option<String>,
}

impl OpenOrder {
    pub fn is_buy(&self) -> bool {
        self.side == "B"
    }
}

/// `userFills` entry and WS `WsFill`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub coin: String,
    #[serde(deserialize_with = "num::f64")]
    pub px: f64,
    #[serde(deserialize_with = "num::f64")]
    pub sz: f64,
    pub side: String,
    pub time: u64,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub start_position: Option<f64>,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub closed_pnl: Option<f64>,
    #[serde(default)]
    pub hash: Option<String>,
    pub oid: u64,
    /// true = taker.
    #[serde(default)]
    pub crossed: bool,
    #[serde(deserialize_with = "num::f64")]
    pub fee: f64,
    pub tid: u64,
    #[serde(default)]
    pub fee_token: Option<String>,
    #[serde(default, deserialize_with = "num::opt_f64")]
    pub builder_fee: Option<f64>,
    #[serde(default)]
    pub cloid: Option<String>,
}

impl Fill {
    pub fn is_buy(&self) -> bool {
        self.side == "B"
    }
}

/// WS `WsOrder` (orderUpdates channel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderUpdate {
    pub order: OpenOrder,
    /// `open` | `filled` | `canceled` | `triggered` | `rejected` | `marginCanceled` | ...
    pub status: String,
    pub status_timestamp: u64,
}

/// WS `WsUserFills`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFills {
    #[serde(default)]
    pub is_snapshot: Option<bool>,
    pub user: String,
    pub fills: Vec<Fill>,
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    /// open time (ms)
    pub t: u64,
    /// close time (ms)
    #[serde(rename = "T")]
    pub close_time: u64,
    pub s: String,
    pub i: String,
    #[serde(deserialize_with = "num::f64")]
    pub o: f64,
    #[serde(deserialize_with = "num::f64")]
    pub c: f64,
    #[serde(deserialize_with = "num::f64")]
    pub h: f64,
    #[serde(deserialize_with = "num::f64")]
    pub l: f64,
    #[serde(deserialize_with = "num::f64")]
    pub v: f64,
    pub n: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingHistoryEntry {
    pub coin: String,
    #[serde(deserialize_with = "num::f64")]
    pub funding_rate: f64,
    /// Hourly premium sample (impact mid vs oracle), fraction.
    #[serde(deserialize_with = "num::f64")]
    pub premium: f64,
    pub time: u64,
}

// ---------------------------------------------------------------------------
// Websocket envelopes
// ---------------------------------------------------------------------------

/// WS `activeAssetCtx` data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WsActiveAssetCtx {
    pub coin: String,
    pub ctx: PerpAssetCtx,
}

/// Parsed websocket message (only the channels the bot uses).
#[derive(Debug, Clone, PartialEq)]
pub enum WsMessage {
    SubscriptionResponse(serde_json::Value),
    Book(L2Book),
    AssetCtx(WsActiveAssetCtx),
    UserFills(UserFills),
    OrderUpdates(Vec<OrderUpdate>),
    Pong,
    Error(String),
    /// Any other channel (ignored).
    Other(String),
}

impl WsMessage {
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct Envelope {
            channel: String,
            #[serde(default)]
            data: serde_json::Value,
        }
        let env: Envelope = serde_json::from_str(text)?;
        Ok(match env.channel.as_str() {
            "subscriptionResponse" => WsMessage::SubscriptionResponse(env.data),
            "l2Book" => WsMessage::Book(serde_json::from_value(env.data)?),
            "activeAssetCtx" => WsMessage::AssetCtx(serde_json::from_value(env.data)?),
            "userFills" => WsMessage::UserFills(serde_json::from_value(env.data)?),
            "orderUpdates" => WsMessage::OrderUpdates(serde_json::from_value(env.data)?),
            "pong" => WsMessage::Pong,
            "error" => WsMessage::Error(match env.data {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            }),
            other => WsMessage::Other(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_accepts_strings_and_numbers() {
        #[derive(Deserialize)]
        struct T {
            #[serde(deserialize_with = "num::f64")]
            a: f64,
            #[serde(default, deserialize_with = "num::opt_f64")]
            b: Option<f64>,
            #[serde(default, deserialize_with = "num::opt_f64")]
            c: Option<f64>,
        }
        let t: T = serde_json::from_str(r#"{"a":"1.25","b":2,"c":null}"#).unwrap();
        assert_eq!((t.a, t.b, t.c), (1.25, Some(2.0), None));
        assert!(serde_json::from_str::<T>(r#"{"a":"x"}"#).is_err());
    }

    #[test]
    fn ws_error_and_unknown_channels() {
        assert_eq!(
            WsMessage::parse(r#"{"channel":"error","data":"bad sub"}"#).unwrap(),
            WsMessage::Error("bad sub".into())
        );
        assert_eq!(
            WsMessage::parse(r#"{"channel":"trades","data":[]}"#).unwrap(),
            WsMessage::Other("trades".into())
        );
        assert_eq!(
            WsMessage::parse(r#"{"channel":"pong"}"#).unwrap(),
            WsMessage::Pong
        );
    }
}
