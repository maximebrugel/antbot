//! Sub-account (vault) routing and native-perp (`dex = ""`) requests against
//! a loopback mock of `/info` + `/exchange` (testnet mode, 127.0.0.1).
//! Nothing leaves the machine.
//!
//! Checks that with a vault configured every info query (positions, open
//! orders, fills, order status, funding) targets the vault, signed actions
//! carry `vaultAddress` and recover to the API wallet with the vault in the
//! hash; that without a vault the main account is queried and
//! `vaultAddress` is `null`; and that native perps resolve on the `""` dex
//! with `perpDexs[0] = null`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use antbot::config::{Config, Mode};
use antbot::exec::live::{HlApi, LiveApi};
use antbot::hl::actions::{Action, Cloid};
use antbot::hl::assets;
use antbot::hl::exchange::{AssetRef, ExchangeClient};
use antbot::hl::info::{InfoClient, TokenBucket};
use antbot::hl::signing::{recover_l1_signer, Address, Signature};
use antbot::mode::{Credentials, PrivateKey};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const KEY: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
const ACCOUNT: &str = "0x00000000000000000000000000000000000000aa";
const VAULT: &str = "0x1719884eb866cb12b2287399b15f7db5e7d775ea";
const BTC: AssetRef = AssetRef {
    asset_id: 0,
    sz_decimals: 5,
};

type Seen = Arc<Mutex<Vec<(String, Value)>>>;

fn reply_for(path: &str, body: &Value) -> Value {
    if path == "/exchange" {
        return json!({"status": "ok", "response": {"type": "default"}});
    }
    match body["type"].as_str().unwrap_or("") {
        "perpDexs" => json!([null, {"name": "xyz", "fullName": "XYZ", "deployer": "0x01"}]),
        "meta" => json!({
            "universe": [
                {"name": "BTC", "szDecimals": 5, "maxLeverage": 40},
                {"name": "ETH", "szDecimals": 4, "maxLeverage": 25}
            ],
            "marginTables": []
        }),
        "clearinghouseState" => json!({
            "marginSummary": {"accountValue": "1000.0", "totalNtlPos": "0.0",
                              "totalRawUsd": "1000.0", "totalMarginUsed": "0.0"},
            "withdrawable": "1000.0",
            "assetPositions": [],
            "time": 1
        }),
        "orderStatus" => json!({"status": "unknownOid"}),
        _ => json!([]),
    }
}

/// HTTP/1.1 mock: records (path, JSON body) and answers by path / type.
async fn spawn_mock() -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = l.accept().await.unwrap();
            let seen = s2.clone();
            tokio::spawn(async move {
                loop {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let (path, body) = loop {
                        let n = match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        let s = String::from_utf8_lossy(&buf).to_string();
                        let Some(h) = s.find("\r\n\r\n") else {
                            continue;
                        };
                        let len = s[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len {
                            let path = s
                                .lines()
                                .next()
                                .and_then(|l| l.split(' ').nth(1))
                                .unwrap_or("")
                                .to_string();
                            let body: Value =
                                serde_json::from_slice(&buf[h + 4..h + 4 + len]).unwrap();
                            break (path, body);
                        }
                    };
                    let reply = reply_for(&path, &body).to_string();
                    seen.lock().unwrap().push((path, body));
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                        reply.len()
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (base, seen)
}

fn info(base: &str) -> InfoClient {
    InfoClient::new(
        format!("{base}/info"),
        Duration::from_secs(5),
        Arc::new(TokenBucket::new(100.0, 20.0)),
    )
    .unwrap()
}

fn client(base: &str, vault: Option<&str>) -> ExchangeClient {
    let cfg = Config::default();
    let mut ep = cfg.endpoints(Mode::Testnet);
    ep.exchange_url = Some(format!("{base}/exchange"));
    ExchangeClient::new(
        Mode::Testnet,
        &ep,
        &Credentials {
            private_key: PrivateKey::parse(KEY).unwrap(),
            account_address: Some(ACCOUNT.into()),
            vault_address: vault.map(|v| v.parse().unwrap()),
        },
        None,
        Duration::from_secs(5),
    )
    .unwrap()
}

async fn exercise(api: &HlApi) {
    api.clearinghouse().await.unwrap();
    api.open_orders().await.unwrap();
    api.user_fills().await.unwrap();
    let _ = api.order_status(Cloid(7)).await;
    api.funding_since(0).await.unwrap();
    api.update_leverage_isolated(1).await.unwrap();
}

fn info_users(seen: &Seen) -> Vec<(String, String, Option<String>)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "/info")
        .map(|(_, b)| {
            (
                b["type"].as_str().unwrap().to_string(),
                b["user"].as_str().unwrap_or("").to_string(),
                b.get("dex").and_then(|d| d.as_str()).map(str::to_string),
            )
        })
        .collect()
}

fn exchange_bodies(seen: &Seen) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "/exchange")
        .map(|(_, b)| b.clone())
        .collect()
}

#[tokio::test]
async fn native_perps_resolve_on_the_empty_dex() {
    let (base, seen) = spawn_mock().await;
    let spec = assets::resolve_live(&info(&base), "", "BTC").await.unwrap();
    assert_eq!(spec.asset_id, 0);
    assert_eq!(spec.dex_index, 0);
    assert_eq!(spec.sz_decimals, 5);
    assert_eq!(spec.funding_multiplier, None);
    let mut cfg = Config::default();
    cfg.market.dex = String::new();
    cfg.market.coin = "BTC".into();
    cfg.market.expected_sz_decimals = 5;
    cfg.market.expected_max_leverage = 40;
    cfg.validate().unwrap();
    assert!(spec.check_expected(&cfg.market).is_empty());
    let bodies: Vec<Value> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, b)| b.clone())
        .collect();
    assert!(bodies.contains(&json!({"type": "perpDexs"})));
    assert!(
        bodies.contains(&json!({"type": "meta", "dex": ""})),
        "{bodies:?}"
    );
}

#[tokio::test]
async fn vault_routes_queries_and_signs_vault_address() {
    let (base, seen) = spawn_mock().await;
    let c = client(&base, Some(VAULT));
    let signer = c.signer_address();
    let api = HlApi::new(c, info(&base), BTC, "", "BTC");
    assert_eq!(api.user, VAULT);
    exercise(&api).await;

    let users = info_users(&seen);
    assert_eq!(users.len(), 5, "{users:?}");
    for (t, u, dex) in &users {
        assert_eq!(u, VAULT, "{t} queried {u}");
        if t == "clearinghouseState" || t == "openOrders" {
            assert_eq!(dex.as_deref(), Some(""), "{t}: native dex");
        }
    }
    let ex = exchange_bodies(&seen);
    assert_eq!(ex.len(), 1);
    let b = &ex[0];
    assert_eq!(b["vaultAddress"], VAULT);
    assert_eq!(b["action"]["type"], "updateLeverage");
    assert_eq!(b["action"]["asset"], 0);
    let action: Action = serde_json::from_value(b["action"].clone()).unwrap();
    let sig: Signature = serde_json::from_value(b["signature"].clone()).unwrap();
    let vault: Address = VAULT.parse().unwrap();
    let nonce = b["nonce"].as_u64().unwrap();
    let rec = recover_l1_signer(&action, &sig, Some(&vault), nonce, None, false).unwrap();
    assert_eq!(rec, signer);
    let wrong = recover_l1_signer(&action, &sig, None, nonce, None, false).unwrap();
    assert_ne!(wrong, signer);
}

#[tokio::test]
async fn without_vault_the_main_account_is_used_and_vault_address_is_null() {
    let (base, seen) = spawn_mock().await;
    let c = client(&base, None);
    let signer = c.signer_address();
    let api = HlApi::new(c, info(&base), BTC, "", "BTC");
    assert_eq!(api.user, ACCOUNT);
    exercise(&api).await;
    assert!(info_users(&seen).iter().all(|(_, u, _)| u == ACCOUNT));
    let ex = exchange_bodies(&seen);
    assert!(ex[0]["vaultAddress"].is_null());
    let action: Action = serde_json::from_value(ex[0]["action"].clone()).unwrap();
    let sig: Signature = serde_json::from_value(ex[0]["signature"].clone()).unwrap();
    let nonce = ex[0]["nonce"].as_u64().unwrap();
    assert_eq!(
        recover_l1_signer(&action, &sig, None, nonce, None, false).unwrap(),
        signer
    );
}
