//! Parsing / asset-resolution tests on JSON captured from the live mainnet
//! info + websocket APIs (2026-11-12, see tests/fixtures/).

use antbot::config::{Config, PremiumSource};
use antbot::hl::assets::{coin_to_asset, perp_asset_id, perp_dex_index, resolve_asset};
use antbot::hl::market::{DataSource, MarketState};
use antbot::hl::types::*;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/",
            $name
        ))
    };
}

fn perp_dexs() -> PerpDexs {
    serde_json::from_str(fixture!("perpDexs.json")).unwrap()
}
fn meta_xyz() -> Meta {
    serde_json::from_str(fixture!("meta_xyz.json")).unwrap()
}

#[test]
fn perp_dexs_layout() {
    let pd = perp_dexs();
    assert!(pd[0].is_none(), "entry 0 is the original dex (null)");
    assert_eq!(pd[1].as_ref().unwrap().name, "xyz");
    assert_eq!(perp_dex_index(&pd, "xyz"), Some(1));
    assert_eq!(perp_dex_index(&pd, ""), Some(0));
    assert_eq!(perp_dex_index(&pd, "nope"), None);
    let xyz = pd[1].as_ref().unwrap();
    assert_eq!(xyz.funding_multiplier("xyz:ANT"), Some(0.005));
    assert_eq!(xyz.streaming_oi_cap("xyz:ANT"), Some(150_000_000.0));
    assert_eq!(xyz.funding_interest_rate("xyz:ANT"), None);
    assert!(xyz.oracle_updater.is_none());
}

#[test]
fn resolve_xyz_ant() {
    let spec = resolve_asset(&perp_dexs(), &meta_xyz(), "xyz", "xyz:ANT").unwrap();
    assert_eq!(spec.dex_index, 1);
    assert_eq!(spec.index_in_meta, 138);
    assert_eq!(spec.asset_id, 110_138);
    assert_eq!(spec.sz_decimals, 2);
    assert_eq!(spec.max_price_decimals, 4);
    assert_eq!(spec.max_leverage, 10);
    assert!(!spec.only_isolated);
    assert_eq!(spec.margin_mode, None);
    assert_eq!(spec.growth_mode.as_deref(), Some("enabled"));
    assert_eq!(spec.deployer_fee_scale, Some(1.0));
    assert_eq!(spec.funding_multiplier, Some(0.005));
    assert!(!spec.is_delisted);
    // Matches the default config baseline.
    assert_eq!(
        spec.check_expected(&Config::default().market),
        Vec::<String>::new()
    );
}

/// Same mapping as the official Python SDK's `Info.coin_to_asset`.
#[test]
fn sdk_equivalent_mapping() {
    let pd = perp_dexs();
    let meta = meta_xyz();
    let map = coin_to_asset(&pd, [("xyz", &meta)]);
    assert_eq!(map["xyz:ANT"], 110_138);
    for (i, a) in meta.universe.iter().enumerate() {
        assert_eq!(map[&a.name], perp_asset_id(1, i), "{}", a.name);
        let spec = resolve_asset(&pd, &meta, "xyz", &a.name).unwrap();
        assert_eq!(spec.asset_id, map[&a.name]);
    }
}

#[test]
fn spec_change_detection() {
    let pd = perp_dexs();
    let base = resolve_asset(&pd, &meta_xyz(), "xyz", "xyz:ANT").unwrap();
    let mut meta2 = meta_xyz();
    let a = &mut meta2.universe[138];
    a.growth_mode = Some("disabled".into());
    a.deployer_fee_scale = Some(2.0);
    let changed = resolve_asset(&pd, &meta2, "xyz", "xyz:ANT").unwrap();
    let d = base.diff(&changed);
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(d.iter().any(|l| l.starts_with("growth_mode")));
    let m = changed.check_expected(&Config::default().market);
    assert_eq!(m.len(), 2, "{m:?}");
    assert!(base.diff(&base).is_empty());
}

#[test]
fn meta_and_asset_ctxs() {
    let mac: MetaAndAssetCtxs =
        serde_json::from_str(fixture!("metaAndAssetCtxs_xyz.json")).unwrap();
    assert_eq!(mac.0.universe.len(), mac.1.len());
    let (i, meta, ctx) = mac.find("xyz:ANT").unwrap();
    assert_eq!(i, 138);
    assert_eq!(meta.sz_decimals, 2);
    assert_eq!(ctx.oracle_px, 104.57);
    assert_eq!(ctx.mark_px, 104.26);
    assert_eq!(ctx.impact_pxs, Some((104.2221, 104.3332)));
    assert_eq!(ctx.premium, Some(-0.0027957349));
    // Our premium formula reproduces the exchange's.
    let mut s = MarketState::new("xyz:ANT");
    s.apply_ctx(ctx, 0, DataSource::Rest);
    let p = s.premium(PremiumSource::Impact).unwrap();
    assert!((p - ctx.premium.unwrap()).abs() < 1e-9, "{p}");
}

#[test]
fn l2_book() {
    let b: L2Book = serde_json::from_str(fixture!("l2Book_ANT.json")).unwrap();
    assert_eq!(b.coin, "xyz:ANT");
    assert_eq!(b.best_bid().unwrap().px, 104.24);
    assert_eq!(b.best_ask().unwrap().px, 104.25);
    assert!(b.bids().windows(2).all(|w| w[0].px > w[1].px));
    assert!(b.asks().windows(2).all(|w| w[0].px < w[1].px));
}

#[test]
fn user_state_payloads() {
    let flat: ClearinghouseState =
        serde_json::from_str(fixture!("clearinghouseState_xyz_flat.json")).unwrap();
    assert!(flat.asset_positions.is_empty());
    assert_eq!(flat.margin_summary.account_value, 2802.5);
    assert!(flat.position("xyz:ANT").is_none());

    let pos: ClearinghouseState =
        serde_json::from_str(fixture!("clearinghouseState_xyz_ANT_pos.json")).unwrap();
    let p = pos.position("xyz:ANT").unwrap();
    assert_eq!(p.szi, 21.98);
    assert_eq!(p.entry_px, Some(104.3576));
    assert_eq!(p.leverage.kind, "cross"); // ANT allows cross margin (no onlyIsolated)
    assert_eq!(p.leverage.value, 10);

    let oo: Vec<OpenOrder> = serde_json::from_str(fixture!("openOrders_xyz_sample.json")).unwrap();
    assert_eq!(oo.len(), 4);
    let ant: Vec<_> = oo.iter().filter(|o| o.coin == "xyz:ANT").collect();
    assert!(!ant.is_empty());
    assert!(!ant[0].is_buy());
    assert!(ant[0].cloid.as_deref().unwrap().starts_with("0x"));
    let empty: Vec<OpenOrder> = serde_json::from_str(fixture!("openOrders_empty.json")).unwrap();
    assert!(empty.is_empty());

    let fills: Vec<Fill> = serde_json::from_str(fixture!("userFills_sample.json")).unwrap();
    assert_eq!(fills.len(), 3);
    assert_eq!(fills[0].px, 769.95);
    assert!(!fills[0].crossed);
}

#[test]
fn history_payloads() {
    let c: Vec<Candle> = serde_json::from_str(fixture!("candleSnapshot_ANT_1h.json")).unwrap();
    assert!(!c.is_empty());
    assert!(c
        .iter()
        .all(|c| c.s == "xyz:ANT" && c.i == "1h" && c.h >= c.l));
    assert_eq!(c[0].close_time, c[0].t + 3_599_999);
    let f: Vec<FundingHistoryEntry> =
        serde_json::from_str(fixture!("fundingHistory_ANT.json")).unwrap();
    assert!(!f.is_empty());
    assert_eq!(f[0].premium, -0.003336963);
}

#[test]
fn ws_messages() {
    let book = WsMessage::parse(fixture!("ws_l2Book.json")).unwrap();
    let WsMessage::Book(b) = book else {
        panic!("{book:?}")
    };
    assert_eq!(b.coin, "xyz:ANT");
    assert!(b.best_bid().unwrap().px < b.best_ask().unwrap().px);

    let ctx = WsMessage::parse(fixture!("ws_activeAssetCtx.json")).unwrap();
    let WsMessage::AssetCtx(c) = ctx else {
        panic!("{ctx:?}")
    };
    assert_eq!(c.coin, "xyz:ANT");
    assert_eq!(c.ctx.oracle_px, 104.57);
    assert_eq!(c.ctx.impact_pxs, Some((104.301, 104.3937)));
    let mut s = MarketState::new("xyz:ANT");
    s.apply_ctx(&c.ctx, 0, DataSource::Ws);
    let p = s.premium(PremiumSource::Impact).unwrap();
    assert!((p - c.ctx.premium.unwrap()).abs() < 1e-9);

    assert_eq!(
        WsMessage::parse(fixture!("ws_pong.json")).unwrap(),
        WsMessage::Pong
    );
    assert!(matches!(
        WsMessage::parse(fixture!("ws_subscriptionResponse.json")).unwrap(),
        WsMessage::SubscriptionResponse(_)
    ));
}

#[test]
fn ws_user_channels() {
    let fills = format!(
        r#"{{"channel":"userFills","data":{{"isSnapshot":true,"user":"0xabc","fills":{}}}}}"#,
        fixture!("userFills_sample.json")
    );
    let WsMessage::UserFills(f) = WsMessage::parse(&fills).unwrap() else {
        panic!()
    };
    assert_eq!(f.is_snapshot, Some(true));
    assert_eq!(f.fills.len(), 3);

    let upd = r#"{"channel":"orderUpdates","data":[{"order":{"coin":"xyz:ANT","side":"A","limitPx":"104.42","sz":"0.0","oid":603115286833,"timestamp":1794510575156,"origSz":"2.6","cloid":"0x00000000179451057493557356728771"},"status":"filled","statusTimestamp":1794510580000}]}"#;
    let WsMessage::OrderUpdates(u) = WsMessage::parse(upd).unwrap() else {
        panic!()
    };
    assert_eq!(u[0].status, "filled");
    assert_eq!(u[0].order.oid, 603115286833);
    assert_eq!(u[0].order.orig_sz, Some(2.6));
}
