//! Watched market spec for the spec-change kill rule.
//!
//! The kill rule compares each meta poll with a **baseline snapshot**: the
//! spec seen at the first start, checked against the `[market] expected_*`
//! values and persisted with the risk state. These fields are watched:
//!
//! | field | source | why |
//! |---|---|---|
//! | `funding_multiplier` | perpDexs.assetToFundingMultiplier | multiplier change |
//! | `oracle_updater`, `deployer` | perpDexs[dex] | oracle source / control change |
//! | `halted` | meta.universe[].isDelisted | halted / delisted market |
//! | `growth_mode`, `deployer_fee_scale`, `last_fee_scale_change_time` | meta.universe[] | fee change |
//! | `sz_decimals` | meta.universe[] | lot / tick change |
//! | `max_leverage`, `margin_mode`, `only_isolated` | meta.universe[] | margin change |
//! | `asset_id` | perpDexs + meta index | asset re-indexing |
//!
//! Hyperliquid's meta has no separate "halted" flag. A halted or delisted
//! HIP-3 asset shows up as `isDelisted`. If the coin disappears from meta, the
//! caller reports it with [`crate::risk::kill::RiskManager::on_spec_unavailable`].

use serde::{Deserialize, Serialize};

use crate::hl::assets::AssetSpec;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchedSpec {
    pub coin: String,
    pub asset_id: u32,
    pub funding_multiplier: Option<f64>,
    pub oracle_updater: Option<String>,
    pub deployer: Option<String>,
    pub halted: bool,
    pub growth_mode: Option<String>,
    pub deployer_fee_scale: Option<f64>,
    pub last_fee_scale_change_time: Option<String>,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    pub margin_mode: Option<String>,
    pub only_isolated: bool,
}

impl From<&AssetSpec> for WatchedSpec {
    fn from(a: &AssetSpec) -> Self {
        Self {
            coin: a.coin.clone(),
            asset_id: a.asset_id,
            funding_multiplier: a.funding_multiplier,
            oracle_updater: a.oracle_updater.clone(),
            deployer: a.deployer.clone(),
            halted: a.is_delisted,
            growth_mode: a.growth_mode.clone(),
            deployer_fee_scale: a.deployer_fee_scale,
            last_fee_scale_change_time: a.last_fee_scale_change_time.clone(),
            sz_decimals: a.sz_decimals,
            max_leverage: a.max_leverage,
            margin_mode: a.margin_mode.clone(),
            only_isolated: a.only_isolated,
        }
    }
}

fn feq(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0),
        (None, None) => true,
        _ => false,
    }
}

fn show<T: std::fmt::Debug>(v: &T) -> String {
    format!("{v:?}")
}

impl WatchedSpec {
    /// Human-readable list of watched fields that differ (`self` = baseline).
    pub fn diff(&self, new: &WatchedSpec) -> Vec<String> {
        let mut out = Vec::new();
        let mut cmp = |name: &str, same: bool, a: String, b: String| {
            if !same {
                out.push(format!("{name}: {a} -> {b}"));
            }
        };
        cmp(
            "coin",
            self.coin == new.coin,
            show(&self.coin),
            show(&new.coin),
        );
        cmp(
            "asset_id",
            self.asset_id == new.asset_id,
            show(&self.asset_id),
            show(&new.asset_id),
        );
        cmp(
            "funding_multiplier",
            feq(self.funding_multiplier, new.funding_multiplier),
            show(&self.funding_multiplier),
            show(&new.funding_multiplier),
        );
        cmp(
            "oracle_updater",
            self.oracle_updater == new.oracle_updater,
            show(&self.oracle_updater),
            show(&new.oracle_updater),
        );
        cmp(
            "deployer",
            self.deployer == new.deployer,
            show(&self.deployer),
            show(&new.deployer),
        );
        cmp(
            "halted",
            self.halted == new.halted,
            show(&self.halted),
            show(&new.halted),
        );
        cmp(
            "growth_mode",
            self.growth_mode == new.growth_mode,
            show(&self.growth_mode),
            show(&new.growth_mode),
        );
        cmp(
            "deployer_fee_scale",
            feq(self.deployer_fee_scale, new.deployer_fee_scale),
            show(&self.deployer_fee_scale),
            show(&new.deployer_fee_scale),
        );
        cmp(
            "last_fee_scale_change_time",
            self.last_fee_scale_change_time == new.last_fee_scale_change_time,
            show(&self.last_fee_scale_change_time),
            show(&new.last_fee_scale_change_time),
        );
        cmp(
            "sz_decimals",
            self.sz_decimals == new.sz_decimals,
            show(&self.sz_decimals),
            show(&new.sz_decimals),
        );
        cmp(
            "max_leverage",
            self.max_leverage == new.max_leverage,
            show(&self.max_leverage),
            show(&new.max_leverage),
        );
        cmp(
            "margin_mode",
            self.margin_mode == new.margin_mode,
            show(&self.margin_mode),
            show(&new.margin_mode),
        );
        cmp(
            "only_isolated",
            self.only_isolated == new.only_isolated,
            show(&self.only_isolated),
            show(&new.only_isolated),
        );
        out
    }
}

#[cfg(test)]
pub(crate) fn sample() -> WatchedSpec {
    WatchedSpec {
        coin: "xyz:ANT".into(),
        asset_id: 110_138,
        funding_multiplier: Some(0.005),
        oracle_updater: None,
        deployer: Some("0x88806a71d74ad0a510b350545c9ae490912f0888".into()),
        halted: false,
        growth_mode: Some("enabled".into()),
        deployer_fee_scale: Some(1.0),
        last_fee_scale_change_time: Some("2026-10-26T00:00:00".into()),
        sz_decimals: 2,
        max_leverage: 10,
        margin_mode: None,
        only_isolated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_has_no_diff() {
        assert!(sample().diff(&sample()).is_empty());
    }

    #[test]
    fn every_watched_field_is_detected() {
        let base = sample();
        type Mutate = Box<dyn Fn(&mut WatchedSpec)>;
        let cases: Vec<(&str, Mutate)> = vec![
            ("coin", Box::new(|s| s.coin = "xyz:ANT2".into())),
            ("asset_id", Box::new(|s| s.asset_id = 110_139)),
            (
                "funding_multiplier",
                Box::new(|s| s.funding_multiplier = Some(0.01)),
            ),
            (
                "oracle_updater",
                Box::new(|s| s.oracle_updater = Some("0xabc".into())),
            ),
            ("deployer", Box::new(|s| s.deployer = None)),
            ("halted", Box::new(|s| s.halted = true)),
            (
                "growth_mode",
                Box::new(|s| s.growth_mode = Some("disabled".into())),
            ),
            (
                "deployer_fee_scale",
                Box::new(|s| s.deployer_fee_scale = Some(2.0)),
            ),
            (
                "last_fee_scale_change_time",
                Box::new(|s| s.last_fee_scale_change_time = Some("2026-11-20T00:00:00".into())),
            ),
            ("sz_decimals", Box::new(|s| s.sz_decimals = 3)),
            ("max_leverage", Box::new(|s| s.max_leverage = 5)),
            (
                "margin_mode",
                Box::new(|s| s.margin_mode = Some("strictIsolated".into())),
            ),
            ("only_isolated", Box::new(|s| s.only_isolated = true)),
        ];
        for (name, f) in cases {
            let mut n = base.clone();
            f(&mut n);
            let d = base.diff(&n);
            assert_eq!(d.len(), 1, "{name}: {d:?}");
            assert!(d[0].starts_with(name), "{name}: {d:?}");
        }
    }

    #[test]
    fn float_noise_and_missing_multiplier() {
        let base = sample();
        let mut n = base.clone();
        n.funding_multiplier = Some(0.005 + 1e-17);
        assert!(base.diff(&n).is_empty());
        n.funding_multiplier = None;
        assert_eq!(base.diff(&n).len(), 1);
    }

    #[test]
    fn from_asset_spec_fixture() {
        let pd: Vec<Option<crate::hl::types::PerpDex>> =
            serde_json::from_str(include_str!("../../tests/fixtures/perpDexs.json")).unwrap();
        let meta: crate::hl::types::Meta =
            serde_json::from_str(include_str!("../../tests/fixtures/meta_xyz.json")).unwrap();
        let a = crate::hl::assets::resolve_asset(&pd, &meta, "xyz", "xyz:ANT").unwrap();
        let w = WatchedSpec::from(&a);
        assert_eq!(w.asset_id, 110_138);
        assert_eq!(w.funding_multiplier, Some(0.005));
        assert_eq!(w.sz_decimals, 2);
        assert_eq!(w.max_leverage, 10);
        assert_eq!(w.growth_mode.as_deref(), Some("enabled"));
        assert!(!w.halted);
    }
}
