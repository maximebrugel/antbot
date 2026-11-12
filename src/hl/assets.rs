//! HIP-3 builder-dex asset resolution (perpDexs + meta{dex}).
//!
//! Official docs (for-developers/api/asset-ids): "Builder-deployed perps
//! expect `100000 + perp_dex_index * 10000 + index_in_meta`", e.g. `test:ABC`
//! on testnet with perp_dex_index 1 / index_in_meta 0 -> 110000.
//!
//! Official Python SDK (`hyperliquid/info.py`, `Info.__init__`):
//! ```python
//! for i, perp_dex in enumerate(self.perp_dexs()[1:]):
//!     # builder-deployed perp dexs start at 110000
//!     perp_dex_to_offset[perp_dex["name"]] = 110000 + i * 10000
//! ...
//! for asset, asset_info in enumerate(meta["universe"]):   # meta(dex=perp_dex)
//!     self.coin_to_asset[asset_info["name"]] = asset + offset
//! ```
//! i.e. `perp_dex_index` is the position in the `perpDexs` response (entry 0
//! is `null` = the original dex, offset 0) and `index_in_meta` the position
//! in `meta{dex}.universe` (delisted entries included). Both agree; this
//! module implements exactly that. See
//! `antbot-work/logs/step2_asset_findings.md` for the live verification
//! (xyz = perpDexs[1], xyz:ANT = universe[138] -> asset 110138).

use serde::{Deserialize, Serialize};

use super::info::InfoClient;
use super::types::{AssetMeta, Meta, PerpDex};
use crate::config::MarketConfig;

/// Max price decimals for perps: `MAX_DECIMALS (6) - szDecimals`.
pub const PERP_MAX_DECIMALS: u32 = 6;

/// Asset id from the perp dex index (0 = original dex) and the index in meta.
pub fn perp_asset_id(perp_dex_index: usize, index_in_meta: usize) -> u32 {
    if perp_dex_index == 0 {
        index_in_meta as u32
    } else {
        (100_000 + perp_dex_index * 10_000 + index_in_meta) as u32
    }
}

/// Position of `dex` in the `perpDexs` response (`""` -> 0).
pub fn perp_dex_index(perp_dexs: &[Option<PerpDex>], dex: &str) -> Option<usize> {
    if dex.is_empty() {
        return Some(0);
    }
    perp_dexs
        .iter()
        .position(|d| d.as_ref().is_some_and(|d| d.name == dex))
}

/// Mapping `coin -> asset id` for the given dexs, built exactly like the
/// Python SDK's `coin_to_asset` (used for cross-checking in tests).
pub fn coin_to_asset<'a>(
    perp_dexs: &[Option<PerpDex>],
    metas: impl IntoIterator<Item = (&'a str, &'a Meta)>,
) -> std::collections::HashMap<String, u32> {
    let mut offsets = std::collections::HashMap::new();
    offsets.insert(String::new(), 0u32);
    for (i, d) in perp_dexs.iter().skip(1).enumerate() {
        if let Some(d) = d {
            offsets.insert(d.name.clone(), 110_000 + i as u32 * 10_000);
        }
    }
    let mut out = std::collections::HashMap::new();
    for (dex, meta) in metas {
        let off = offsets[dex];
        for (a, info) in meta.universe.iter().enumerate() {
            out.insert(info.name.clone(), a as u32 + off);
        }
    }
    out
}

/// Everything the bot needs to know about the traded asset; also the
/// snapshot compared on each meta poll for the spec-change kill rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetSpec {
    pub coin: String,
    pub dex: String,
    pub dex_index: usize,
    pub index_in_meta: usize,
    pub asset_id: u32,
    pub sz_decimals: u32,
    /// `6 - szDecimals` (prices also limited to 5 significant figures).
    pub max_price_decimals: u32,
    pub max_leverage: u32,
    pub margin_table_id: Option<u32>,
    pub only_isolated: bool,
    pub margin_mode: Option<String>,
    pub growth_mode: Option<String>,
    pub deployer_fee_scale: Option<f64>,
    pub last_fee_scale_change_time: Option<String>,
    pub is_delisted: bool,
    pub funding_multiplier: Option<f64>,
    pub funding_interest_rate: Option<f64>,
    pub funding_clamp: Option<serde_json::Value>,
    pub streaming_oi_cap: Option<f64>,
    pub oracle_updater: Option<String>,
    pub deployer: Option<String>,
    pub fee_recipient: Option<String>,
    pub collateral_token: Option<u32>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ResolveError {
    #[error("perp dex {0:?} not found in perpDexs")]
    DexNotFound(String),
    #[error("coin {coin:?} not found in meta for dex {dex:?}")]
    CoinNotFound { dex: String, coin: String },
    #[error("coin {coin:?} does not belong to dex {dex:?} (expected \"{dex}:<coin>\")")]
    BadCoinName { dex: String, coin: String },
}

/// Resolve `coin` on `dex` from `perpDexs` and `meta{dex}`.
pub fn resolve_asset(
    perp_dexs: &[Option<PerpDex>],
    meta: &Meta,
    dex: &str,
    coin: &str,
) -> Result<AssetSpec, ResolveError> {
    if !dex.is_empty() && !coin.starts_with(&format!("{dex}:")) {
        return Err(ResolveError::BadCoinName {
            dex: dex.into(),
            coin: coin.into(),
        });
    }
    let dex_index =
        perp_dex_index(perp_dexs, dex).ok_or_else(|| ResolveError::DexNotFound(dex.into()))?;
    let index_in_meta = meta
        .index_of(coin)
        .ok_or_else(|| ResolveError::CoinNotFound {
            dex: dex.into(),
            coin: coin.into(),
        })?;
    let a: &AssetMeta = &meta.universe[index_in_meta];
    let pd = perp_dexs.get(dex_index).and_then(|d| d.as_ref());
    Ok(AssetSpec {
        coin: coin.into(),
        dex: dex.into(),
        dex_index,
        index_in_meta,
        asset_id: perp_asset_id(dex_index, index_in_meta),
        sz_decimals: a.sz_decimals,
        max_price_decimals: PERP_MAX_DECIMALS.saturating_sub(a.sz_decimals),
        max_leverage: a.max_leverage,
        margin_table_id: a.margin_table_id,
        only_isolated: a.only_isolated.unwrap_or(false),
        margin_mode: a.margin_mode.clone(),
        growth_mode: a.growth_mode.clone(),
        deployer_fee_scale: a.deployer_fee_scale,
        last_fee_scale_change_time: a.last_fee_scale_change_time.clone(),
        is_delisted: a.is_delisted.unwrap_or(false),
        funding_multiplier: pd.and_then(|d| d.funding_multiplier(coin)),
        funding_interest_rate: pd.and_then(|d| d.funding_interest_rate(coin)),
        funding_clamp: pd.and_then(|d| d.funding_clamp(coin)),
        streaming_oi_cap: pd.and_then(|d| d.streaming_oi_cap(coin)),
        oracle_updater: pd.and_then(|d| d.oracle_updater.clone()),
        deployer: pd.and_then(|d| d.deployer.clone()),
        fee_recipient: pd.and_then(|d| d.fee_recipient.clone()),
        collateral_token: meta.collateral_token,
    })
}

/// Fetch `perpDexs` + `meta{dex}` and resolve the asset.
pub async fn resolve_live(info: &InfoClient, dex: &str, coin: &str) -> anyhow::Result<AssetSpec> {
    let (pd, meta) = tokio::try_join!(info.perp_dexs(), info.meta(dex))?;
    Ok(resolve_asset(&pd, &meta, dex, coin)?)
}

fn feq(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0)
}

fn fmt_opt<T: std::fmt::Debug>(v: &Option<T>) -> String {
    match v {
        Some(v) => format!("{v:?}"),
        None => "none".into(),
    }
}

impl AssetSpec {
    /// Mismatches against the configured baseline (`[market]` expected_*).
    pub fn check_expected(&self, m: &MarketConfig) -> Vec<String> {
        let mut out = Vec::new();
        if self.coin != m.coin || self.dex != m.dex {
            out.push(format!(
                "market {}/{} != configured {}/{}",
                self.dex, self.coin, m.dex, m.coin
            ));
        }
        match self.funding_multiplier {
            Some(x) if feq(x, m.expected_funding_multiplier) => {}
            other => out.push(format!(
                "funding multiplier {} != expected {}",
                fmt_opt(&other),
                m.expected_funding_multiplier
            )),
        }
        if self.growth_mode.as_deref() != Some(m.expected_growth_mode.as_str()) {
            out.push(format!(
                "growthMode {} != expected {:?}",
                fmt_opt(&self.growth_mode),
                m.expected_growth_mode
            ));
        }
        match self.deployer_fee_scale {
            Some(x) if feq(x, m.expected_deployer_fee_scale) => {}
            other => out.push(format!(
                "deployerFeeScale {} != expected {}",
                fmt_opt(&other),
                m.expected_deployer_fee_scale
            )),
        }
        if self.sz_decimals != m.expected_sz_decimals {
            out.push(format!(
                "szDecimals {} != expected {}",
                self.sz_decimals, m.expected_sz_decimals
            ));
        }
        if self.max_leverage != m.expected_max_leverage {
            out.push(format!(
                "maxLeverage {} != expected {}",
                self.max_leverage, m.expected_max_leverage
            ));
        }
        if self.is_delisted {
            out.push("asset is delisted".into());
        }
        out
    }

    /// Field-level changes between two snapshots (`self` = previous).
    pub fn diff(&self, new: &AssetSpec) -> Vec<String> {
        let a = serde_json::to_value(self).unwrap_or_default();
        let b = serde_json::to_value(new).unwrap_or_default();
        let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
            return vec![];
        };
        let mut out: Vec<String> = a
            .iter()
            .filter(|(k, v)| b.get(*k) != Some(*v))
            .map(|(k, v)| format!("{k}: {v} -> {}", b.get(k).cloned().unwrap_or_default()))
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_id_formula() {
        assert_eq!(perp_asset_id(0, 0), 0); // BTC on the original dex
        assert_eq!(perp_asset_id(0, 5), 5);
        assert_eq!(perp_asset_id(1, 0), 110_000); // docs: test:ABC on testnet
        assert_eq!(perp_asset_id(1, 138), 110_138);
        assert_eq!(perp_asset_id(3, 7), 130_007);
    }

    #[test]
    fn bad_coin_prefix() {
        let meta = Meta {
            universe: vec![],
            margin_tables: serde_json::Value::Null,
            collateral_token: None,
        };
        assert_eq!(
            resolve_asset(&[None], &meta, "xyz", "ANT"),
            Err(ResolveError::BadCoinName {
                dex: "xyz".into(),
                coin: "ANT".into()
            })
        );
    }
}
