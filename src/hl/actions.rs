//! `/exchange` action wire types.
//!
//! Field declaration order == key order of the dicts built by the official
//! Python SDK (`order_request_to_order_wire`, `order_wires_to_order_action`,
//! `bulk_cancel`, `bulk_cancel_by_cloid`, `update_leverage`,
//! `schedule_cancel`). The msgpack hash depends on it, so do not reorder.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::signing::{float_to_wire, SigningError};

/// Time in force of a limit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tif {
    /// Add-liquidity-only (post-only): rejected if it would cross.
    Alo,
    /// Immediate-or-cancel (taker).
    Ioc,
    /// Good-til-cancelled.
    Gtc,
}

impl Tif {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tif::Alo => "Alo",
            Tif::Ioc => "Ioc",
            Tif::Gtc => "Gtc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitWire {
    pub tif: Tif,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tpsl {
    Tp,
    Sl,
}

/// Trigger order wire (SDK order: isMarket, triggerPx, tpsl). Not used by the
/// strategy; kept so the SDK tpsl vector can be reproduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerWire {
    #[serde(rename = "isMarket")]
    pub is_market: bool,
    #[serde(rename = "triggerPx")]
    pub trigger_px: String,
    pub tpsl: Tpsl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderTypeWire {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub limit: Option<LimitWire>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trigger: Option<TriggerWire>,
}

impl OrderTypeWire {
    pub fn limit(tif: Tif) -> Self {
        Self {
            limit: Some(LimitWire { tif }),
            trigger: None,
        }
    }
}

/// Client order id: 16 bytes, wire form `0x` + 32 lowercase hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cloid(pub u128);

impl Cloid {
    pub fn to_raw(&self) -> String {
        format!("{:#034x}", self.0)
    }
}

impl fmt::Display for Cloid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_raw())
    }
}

impl fmt::Debug for Cloid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_raw())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("cloid must be 0x + 32 hex chars, got {0:?}")]
pub struct CloidError(pub String);

impl FromStr for Cloid {
    type Err = CloidError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let h = s.strip_prefix("0x").ok_or_else(|| CloidError(s.into()))?;
        if h.len() != 32 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(CloidError(s.into()));
        }
        u128::from_str_radix(h, 16)
            .map(Cloid)
            .map_err(|_| CloidError(s.into()))
    }
}

impl Serialize for Cloid {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_raw())
    }
}

impl<'de> Deserialize<'de> for Cloid {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// One order on the wire (`a, b, p, s, r, t, c`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderWire {
    /// Asset id (HIP-3: 100000 + dex_index*10000 + index_in_meta).
    pub a: u32,
    /// is_buy.
    pub b: bool,
    /// Limit price (`float_to_wire`).
    pub p: String,
    /// Size (`float_to_wire`).
    pub s: String,
    /// reduce_only.
    pub r: bool,
    pub t: OrderTypeWire,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub c: Option<Cloid>,
}

impl OrderWire {
    /// SDK `order_request_to_order_wire` for a limit order. `px`/`sz` must
    /// already be rounded to the asset's tick/lot (see `crate::hl::rounding`).
    pub fn limit(
        asset: u32,
        is_buy: bool,
        px: f64,
        sz: f64,
        reduce_only: bool,
        tif: Tif,
        cloid: Option<Cloid>,
    ) -> Result<Self, SigningError> {
        Ok(Self {
            a: asset,
            b: is_buy,
            p: float_to_wire(px)?,
            s: float_to_wire(sz)?,
            r: reduce_only,
            t: OrderTypeWire::limit(tif),
            c: cloid,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grouping {
    #[serde(rename = "na")]
    Na,
    #[serde(rename = "normalTpsl")]
    NormalTpsl,
    #[serde(rename = "positionTpsl")]
    PositionTpsl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelWire {
    pub a: u32,
    pub o: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelByCloidWire {
    pub asset: u32,
    pub cloid: Cloid,
}

/// L1 actions used by the bot. Internally tagged so `type` is the first key,
/// as in the SDK dicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Action {
    #[serde(rename = "order")]
    Order {
        orders: Vec<OrderWire>,
        grouping: Grouping,
    },
    #[serde(rename = "cancel")]
    Cancel { cancels: Vec<CancelWire> },
    #[serde(rename = "cancelByCloid")]
    CancelByCloid { cancels: Vec<CancelByCloidWire> },
    #[serde(rename = "updateLeverage")]
    UpdateLeverage {
        asset: u32,
        #[serde(rename = "isCross")]
        is_cross: bool,
        leverage: u32,
    },
    #[serde(rename = "scheduleCancel")]
    ScheduleCancel {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        time: Option<u64>,
    },
}

impl Action {
    pub fn type_name(&self) -> &'static str {
        match self {
            Action::Order { .. } => "order",
            Action::Cancel { .. } => "cancel",
            Action::CancelByCloid { .. } => "cancelByCloid",
            Action::UpdateLeverage { .. } => "updateLeverage",
            Action::ScheduleCancel { .. } => "scheduleCancel",
        }
    }

    /// Number of items in a batched action (for the rate-limit weight).
    pub fn batch_len(&self) -> usize {
        match self {
            Action::Order { orders, .. } => orders.len(),
            Action::Cancel { cancels } => cancels.len(),
            Action::CancelByCloid { cancels } => cancels.len(),
            _ => 1,
        }
    }

    /// SDK `order_wires_to_order_action(order_wires)` (no builder, "na").
    pub fn orders(orders: Vec<OrderWire>) -> Self {
        Action::Order {
            orders,
            grouping: Grouping::Na,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Order vectors from the official SDK `tests/signing_test.py`
    //! (commit 2fdb18f9517675ea03695a0962bd19eece9c83f0).
    use super::*;
    use crate::hl::signing::{action_hash, phantom_source, sign_l1_action, Signature, Wallet};

    const KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

    fn check(sig: Signature, r: &str, s: &str, v: u8) {
        assert_eq!(
            (sig.r_hex().as_str(), sig.s_hex().as_str(), sig.v),
            (r, s, v)
        );
    }

    #[test]
    fn test_phantom_agent_creation_matches_production() {
        // ETH (asset 4) IOC buy 0.0147 @ 1670.1, nonce 1677777606040, mainnet.
        let action = Action::orders(vec![OrderWire::limit(
            4,
            true,
            1670.1,
            0.0147,
            false,
            Tif::Ioc,
            None,
        )
        .unwrap()]);
        let h = action_hash(&action, None, 1677777606040, None).unwrap();
        assert_eq!(
            format!("0x{}", hex::encode(h)),
            "0x0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908"
        );
        assert_eq!(phantom_source(true), "a");
    }

    #[test]
    fn test_l1_action_signing_order_matches() {
        let w = Wallet::from_hex(KEY).unwrap();
        let action = Action::orders(vec![OrderWire::limit(
            1,
            true,
            100.0,
            100.0,
            false,
            Tif::Gtc,
            None,
        )
        .unwrap()]);
        check(
            sign_l1_action(&w, &action, None, 0, None, true).unwrap(),
            "0xd65369825a9df5d80099e513cce430311d7d26ddf477f5b3a33d2806b100d78e",
            "0x2b54116ff64054968aa237c20ca9ff68000f977c93289157748a3162b6ea940e",
            28,
        );
        check(
            sign_l1_action(&w, &action, None, 0, None, false).unwrap(),
            "0x82b2ba28e76b3d761093aaded1b1cdad4960b3af30212b343fb2e6cdfa4e3d54",
            "0x6b53878fc99d26047f4d7e8c90eb98955a109f44209163f52d8dc4278cbbd9f5",
            27,
        );
    }

    #[test]
    fn test_l1_action_signing_order_with_cloid_matches() {
        let w = Wallet::from_hex(KEY).unwrap();
        let cloid: Cloid = "0x00000000000000000000000000000001".parse().unwrap();
        let action = Action::orders(vec![OrderWire::limit(
            1,
            true,
            100.0,
            100.0,
            false,
            Tif::Gtc,
            Some(cloid),
        )
        .unwrap()]);
        check(
            sign_l1_action(&w, &action, None, 0, None, true).unwrap(),
            "0x41ae18e8239a56cacbc5dad94d45d0b747e5da11ad564077fcac71277a946e3",
            "0x3c61f667e747404fe7eea8f90ab0e76cc12ce60270438b2058324681a00116da",
            27,
        );
        check(
            sign_l1_action(&w, &action, None, 0, None, false).unwrap(),
            "0xeba0664bed2676fc4e5a743bf89e5c7501aa6d870bdb9446e122c9466c5cd16d",
            "0x7f3e74825c9114bc59086f1eebea2928c190fdfbfde144827cb02b85bbe90988",
            28,
        );
    }

    #[test]
    fn test_l1_action_signing_tpsl_order_matches() {
        let w = Wallet::from_hex(KEY).unwrap();
        let action = Action::orders(vec![OrderWire {
            a: 1,
            b: true,
            p: "100".into(),
            s: "100".into(),
            r: false,
            t: OrderTypeWire {
                limit: None,
                trigger: Some(TriggerWire {
                    is_market: true,
                    trigger_px: float_to_wire(103.0).unwrap(),
                    tpsl: Tpsl::Sl,
                }),
            },
            c: None,
        }]);
        check(
            sign_l1_action(&w, &action, None, 0, None, true).unwrap(),
            "0x98343f2b5ae8e26bb2587daad3863bc70d8792b09af1841b6fdd530a2065a3f9",
            "0x6b5bb6bb0633b710aa22b721dd9dee6d083646a5f8e581a20b545be6c1feb405",
            27,
        );
        check(
            sign_l1_action(&w, &action, None, 0, None, false).unwrap(),
            "0x971c554d917c44e0e1b6cc45d8f9404f32172a9d3b3566262347d0302896a2e4",
            "0x206257b104788f80450f8e786c329daa589aa0b32ba96948201ae556d5637eac",
            28,
        );
    }

    #[test]
    fn json_shape_matches_sdk_dicts() {
        let cloid: Cloid = "0x0000019a7a2b3c4d00000000deadbeef".parse().unwrap();
        let a = Action::orders(vec![OrderWire::limit(
            110138,
            true,
            104.57,
            23.91,
            false,
            Tif::Alo,
            Some(cloid),
        )
        .unwrap()]);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"type":"order","orders":[{"a":110138,"b":true,"p":"104.57","s":"23.91","r":false,"t":{"limit":{"tif":"Alo"}},"c":"0x0000019a7a2b3c4d00000000deadbeef"}],"grouping":"na"}"#
        );
        let l = Action::UpdateLeverage {
            asset: 110138,
            is_cross: false,
            leverage: 1,
        };
        assert_eq!(
            serde_json::to_string(&l).unwrap(),
            r#"{"type":"updateLeverage","asset":110138,"isCross":false,"leverage":1}"#
        );
        // JSON round trip
        let back: Action = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn cloid_parsing() {
        let c: Cloid = "0x0123456789abcdef0123456789abcdef".parse().unwrap();
        assert_eq!(c.to_raw(), "0x0123456789abcdef0123456789abcdef");
        assert_eq!(Cloid(1).to_raw(), "0x00000000000000000000000000000001");
        assert!("0x01".parse::<Cloid>().is_err());
        assert!("0123456789abcdef0123456789abcdef".parse::<Cloid>().is_err());
        assert!("0xzz23456789abcdef0123456789abcdef"
            .parse::<Cloid>()
            .is_err());
    }
}
