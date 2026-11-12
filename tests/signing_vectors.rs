//! Vectors generated with the official hyperliquid-python-sdk 0.24.0
//! (`/workspace/antbot-work/pysdk/gen_vectors.py`, dummy keys only, no
//! network). Fixture: `tests/fixtures/signing_vectors_pysdk.json`.
//!
//! Covers HIP-3 asset 110138 (xyz:ANT): ALO buy/sell, IOC reduce-only (main
//! key and an agent-style key), cancel, cancelByCloid, updateLeverage
//! isolated, expiresAfter and vault+expiresAfter hashing; float_to_wire on
//! 321 values; the perp price rounding formula on 460 (px, szDecimals) pairs.

use antbot::hl::actions::{Action, CancelByCloidWire, CancelWire, Cloid, OrderWire, Tif};
use antbot::hl::exchange::{AssetRef, OrderRequest};
use antbot::hl::rounding::{round_price, RoundDir};
use antbot::hl::signing::{
    action_hash, float_to_wire, msgpack_named, recover_l1_signer, sign_l1_action, Address, Wallet,
};
use serde_json::Value;

const ANT: AssetRef = AssetRef {
    asset_id: 110138,
    sz_decimals: 2,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/signing_vectors_pysdk.json"
    )))
    .unwrap()
}

fn vector<'a>(fx: &'a Value, name: &str) -> &'a Value {
    fx["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name)
        .unwrap_or_else(|| panic!("vector {name}"))
}

fn wallet_for(fx: &Value, signer: &str) -> Wallet {
    for k in ["key", "agent_key"] {
        let w = Wallet::from_hex(fx["provenance"][k].as_str().unwrap()).unwrap();
        if w.address().to_hex() == signer {
            return w;
        }
    }
    panic!("no key for {signer}");
}

#[test]
fn addresses_match_eth_account() {
    let fx = fixture();
    for k in ["key", "agent_key"] {
        let w = Wallet::from_hex(fx["provenance"][k].as_str().unwrap()).unwrap();
        assert_eq!(w.address().to_hex(), fx["addresses"][k].as_str().unwrap());
    }
}

/// Every SDK vector: JSON action -> typed Action -> msgpack bytes, hash and
/// both signatures identical to the SDK.
#[test]
fn all_vectors_reproduce_sdk() {
    let fx = fixture();
    let vs = fx["vectors"].as_array().unwrap();
    assert_eq!(vs.len(), 9);
    for v in vs {
        let name = v["name"].as_str().unwrap();
        let action: Action = serde_json::from_value(v["action"].clone()).unwrap();
        // Round trip back to the exact same JSON (no field lost/added).
        assert_eq!(
            serde_json::to_value(&action).unwrap(),
            v["action"],
            "{name}"
        );
        assert_eq!(
            hex::encode(msgpack_named(&action).unwrap()),
            v["msgpack_hex"].as_str().unwrap(),
            "{name}: msgpack"
        );
        let vault: Option<Address> = v["vault"].as_str().map(|s| s.parse().unwrap());
        let nonce = v["nonce"].as_u64().unwrap();
        let exp = v["expires_after"].as_u64();
        let h = action_hash(&action, vault.as_ref(), nonce, exp).unwrap();
        assert_eq!(
            format!("0x{}", hex::encode(h)),
            v["action_hash"].as_str().unwrap(),
            "{name}: hash"
        );
        let w = wallet_for(&fx, v["signer"].as_str().unwrap());
        for (net, mainnet) in [("mainnet", true), ("testnet", false)] {
            let sig = sign_l1_action(&w, &action, vault.as_ref(), nonce, exp, mainnet).unwrap();
            let want = &v["sig"][net];
            assert_eq!(sig.r_hex(), want["r"].as_str().unwrap(), "{name} {net} r");
            assert_eq!(sig.s_hex(), want["s"].as_str().unwrap(), "{name} {net} s");
            assert_eq!(sig.v as u64, want["v"].as_u64().unwrap(), "{name} {net} v");
            assert_eq!(
                serde_json::to_value(sig).unwrap(),
                *want,
                "{name} {net} json"
            );
            assert_eq!(
                recover_l1_signer(&action, &sig, vault.as_ref(), nonce, exp, mainnet).unwrap(),
                w.address()
            );
        }
    }
}

/// The typed builders used by the bot produce the SDK actions.
#[test]
fn builders_produce_sdk_actions() {
    let fx = fixture();
    let parse = |name: &str| -> Action {
        serde_json::from_value(vector(&fx, name)["action"].clone()).unwrap()
    };
    let cloid: Cloid = "0x0000019a7a2b3c4d00000000deadbeef".parse().unwrap();

    let alo_buy = OrderRequest::alo(ANT, true, 104.57, 23.91, false, Some(cloid)).unwrap();
    assert_eq!(
        Action::orders(vec![alo_buy.to_wire(ANT).unwrap()]),
        parse("alo_buy_ant_cloid")
    );

    let alo_sell = OrderRequest::alo(ANT, false, 105.1, 24.0, false, None).unwrap();
    assert_eq!(
        Action::orders(vec![alo_sell.to_wire(ANT).unwrap()]),
        parse("alo_sell_ant_nocloid")
    );

    let ioc = OrderRequest::ioc(
        ANT,
        false,
        103.52,
        23.91,
        true,
        Some("0x00000000000000000000000000000002".parse().unwrap()),
    )
    .unwrap();
    assert_eq!(ioc.tif, Tif::Ioc);
    assert_eq!(
        Action::orders(vec![ioc.to_wire(ANT).unwrap()]),
        parse("ioc_sell_reduce_only_ant")
    );

    let agent_ioc = OrderWire::limit(
        110138,
        true,
        99999.0,
        0.01,
        true,
        Tif::Ioc,
        Some("0x0123456789abcdef0123456789abcdef".parse().unwrap()),
    )
    .unwrap();
    assert_eq!(
        Action::orders(vec![agent_ioc]),
        parse("ioc_buy_reduce_only_ant_agent")
    );

    assert_eq!(
        Action::Cancel {
            cancels: vec![CancelWire {
                a: 110138,
                o: 123456789012
            }]
        },
        parse("cancel_ant")
    );
    assert_eq!(
        Action::CancelByCloid {
            cancels: vec![CancelByCloidWire {
                asset: 110138,
                cloid
            }]
        },
        parse("cancel_by_cloid_ant")
    );
    assert_eq!(
        Action::UpdateLeverage {
            asset: 110138,
            is_cross: false,
            leverage: 1
        },
        parse("update_leverage_isolated_1x_ant")
    );
}

#[test]
fn float_to_wire_matches_sdk() {
    let fx = fixture();
    let mut n = 0;
    for c in fx["float_to_wire"].as_array().unwrap() {
        let x = c["x"].as_f64().unwrap();
        let got = float_to_wire(x).ok();
        let want = c["wire"].as_str().map(str::to_string);
        // Only known deviation: SDK gives "-0" for -0.0, we give "0".
        if x == 0.0 && x.is_sign_negative() {
            assert_eq!(want.as_deref(), Some("-0"));
            assert_eq!(got.as_deref(), Some("0"));
            continue;
        }
        assert_eq!(got, want, "float_to_wire({x:?})");
        n += 1;
    }
    assert!(n > 300);
}

#[test]
fn nearest_price_rounding_matches_sdk_formula() {
    let fx = fixture();
    let cases = fx["perp_price_round"].as_array().unwrap();
    assert_eq!(cases.len(), 460);
    for c in cases {
        let px = c["px"].as_f64().unwrap();
        let szd = c["sz_decimals"].as_u64().unwrap() as u32;
        let want = c["rounded"].as_f64().unwrap();
        if want == 0.0 {
            // Below one tick: we refuse instead of sending a 0 price.
            assert!(
                round_price(px, szd, RoundDir::Nearest).is_err(),
                "{px} {szd}"
            );
            continue;
        }
        let got = round_price(px, szd, RoundDir::Nearest).unwrap();
        assert_eq!(got, want, "round_price({px}, {szd})");
    }
}
