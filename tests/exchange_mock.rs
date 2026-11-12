//! Exchange client against a loopback mock `/exchange` server (testnet mode,
//! http://127.0.0.1). Nothing leaves the machine. Checks the posted body
//! (action, nonce, signature recoverable to the signer with the testnet
//! phantom agent, vaultAddress null) and response handling.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use antbot::config::{Config, Mode};
use antbot::hl::actions::{Action, Cloid};
use antbot::hl::exchange::{
    AssetRef, CancelStatus, ExchangeClient, ExchangeError, OrderRequest, OrderStatus,
};
use antbot::hl::signing::{recover_l1_signer, Signature};
use antbot::mode::Credentials;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const KEY: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
const ACCOUNT: &str = "0x00000000000000000000000000000000000000aa";
const ANT: AssetRef = AssetRef {
    asset_id: 110138,
    sz_decimals: 2,
};

type Seen = Arc<Mutex<Vec<Value>>>;

/// Answers each POST with the next canned (status, body); records bodies.
async fn spawn_mock(replies: Vec<(&'static str, String)>) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/exchange", l.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let s2 = seen.clone();
    let replies = Arc::new(Mutex::new(replies.into_iter()));
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = l.accept().await.unwrap();
            let seen = s2.clone();
            let replies = replies.clone();
            tokio::spawn(async move {
                loop {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let body = loop {
                        let n = match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        let s = String::from_utf8_lossy(&buf).to_string();
                        if let Some(h) = s.find("\r\n\r\n") {
                            let len = s[..h]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if buf.len() >= h + 4 + len {
                                break buf[h + 4..h + 4 + len].to_vec();
                            }
                        }
                    };
                    seen.lock()
                        .unwrap()
                        .push(serde_json::from_slice(&body).unwrap());
                    let (status, payload) = replies
                        .lock()
                        .unwrap()
                        .next()
                        .unwrap_or(("500 Internal Server Error", "no reply".into()));
                    let resp = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{payload}",
                        payload.len()
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, seen)
}

fn client(url: &str) -> ExchangeClient {
    let cfg = Config::default();
    let mut ep = cfg.endpoints(Mode::Testnet);
    ep.exchange_url = Some(url.to_string());
    ExchangeClient::new(
        Mode::Testnet,
        &ep,
        &Credentials {
            private_key: KEY.into(),
            account_address: Some(ACCOUNT.into()),
        },
        None,
        Duration::from_secs(5),
    )
    .unwrap()
}

/// Verify the posted body is a correctly signed testnet request.
fn verify_body(c: &ExchangeClient, body: &Value) -> Action {
    let action: Action = serde_json::from_value(body["action"].clone()).unwrap();
    let sig: Signature = serde_json::from_value(body["signature"].clone()).unwrap();
    let nonce = body["nonce"].as_u64().unwrap();
    assert!(body["vaultAddress"].is_null());
    let exp = body["expiresAfter"].as_u64();
    let signer = recover_l1_signer(&action, &sig, None, nonce, exp, false).unwrap();
    assert_eq!(signer, c.signer_address(), "signed by the agent key");
    // Mainnet phantom agent would recover a different address.
    assert_ne!(
        recover_l1_signer(&action, &sig, None, nonce, exp, true).unwrap(),
        c.signer_address()
    );
    action
}

fn ok(json: &str) -> (&'static str, String) {
    ("200 OK", json.to_string())
}

#[tokio::test]
async fn order_cancel_leverage_roundtrip() {
    let (url, seen) = spawn_mock(vec![
        ok(r#"{"status":"ok","response":{"type":"default"}}"#),
        ok(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":42,"cloid":"0x00000000000000000000000000000007"}}]}}}"#,
        ),
        ok(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Post only order would have immediately matched, bbo was 104.5@104.6. asset=110138"}]}}}"#,
        ),
        ok(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"totalSz":"23.9","avgPx":"104.41","oid":43}}]}}}"#,
        ),
        ok(r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#),
        ok(
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[{"error":"Order was never placed, already canceled, or filled. asset=110138"}]}}}"#,
        ),
    ])
    .await;
    let c = client(&url).with_expires_after_window(Some(60_000));
    assert!(c.is_agent());

    c.update_leverage_isolated(ANT, 1).await.unwrap();

    let cloid = Cloid(7);
    let buy = OrderRequest::alo(ANT, true, 104.4789, 23.9, false, Some(cloid)).unwrap();
    let st = c.order(ANT, &buy).await.unwrap();
    assert_eq!(
        st,
        OrderStatus::Resting {
            oid: 42,
            cloid: Some(cloid)
        }
    );

    let st = c.order(ANT, &buy).await.unwrap();
    assert!(st.is_post_only_reject());

    let exit = OrderRequest::ioc(ANT, false, 104.0, 23.9, true, None).unwrap();
    let st = c.order(ANT, &exit).await.unwrap();
    assert!(matches!(st, OrderStatus::Filled { oid: 43, total_sz, .. } if total_sz == 23.9));

    assert_eq!(c.cancel(ANT, 42).await.unwrap(), CancelStatus::Success);
    assert!(matches!(
        c.cancel_by_cloid(ANT, cloid).await.unwrap(),
        CancelStatus::Error(_)
    ));

    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 6);
    let mut last_nonce = 0;
    for b in &bodies {
        verify_body(&c, b);
        let n = b["nonce"].as_u64().unwrap();
        assert!(n > last_nonce, "nonces strictly increase");
        assert_eq!(b["expiresAfter"].as_u64().unwrap(), n + 60_000);
        last_nonce = n;
    }
    assert_eq!(
        bodies[0]["action"],
        serde_json::json!({"type":"updateLeverage","asset":110138,"isCross":false,"leverage":1})
    );
    assert_eq!(
        bodies[1]["action"],
        serde_json::json!({"type":"order","orders":[{"a":110138,"b":true,"p":"104.47","s":"23.9","r":false,"t":{"limit":{"tif":"Alo"}},"c":"0x00000000000000000000000000000007"}],"grouping":"na"})
    );
    assert_eq!(
        bodies[3]["action"]["orders"][0],
        serde_json::json!({"a":110138,"b":false,"p":"104","s":"23.9","r":true,"t":{"limit":{"tif":"Ioc"}}})
    );
    assert_eq!(
        bodies[4]["action"],
        serde_json::json!({"type":"cancel","cancels":[{"a":110138,"o":42}]})
    );
    assert_eq!(
        bodies[5]["action"],
        serde_json::json!({"type":"cancelByCloid","cancels":[{"asset":110138,"cloid":"0x00000000000000000000000000000007"}]})
    );
}

#[tokio::test]
async fn errors_are_surfaced_not_retried() {
    let (url, seen) = spawn_mock(vec![
        ok(r#"{"status":"err","response":"User or API Wallet 0x19e7 does not exist."}"#),
        ("429 Too Many Requests", "null".into()),
        ok("not json"),
    ])
    .await;
    let c = client(&url);
    let o = OrderRequest::alo(ANT, true, 104.0, 1.0, false, None).unwrap();
    assert!(
        matches!(c.order(ANT, &o).await, Err(ExchangeError::Api(m)) if m.contains("does not exist"))
    );
    assert!(matches!(
        c.order(ANT, &o).await,
        Err(ExchangeError::Http { status: 429, .. })
    ));
    assert!(matches!(
        c.cancel(ANT, 1).await,
        Err(ExchangeError::Decode(_))
    ));
    // Exactly one POST per call: orders are never retried.
    assert_eq!(seen.lock().unwrap().len(), 3);
    // Invalid orders never reach the network.
    let tiny = OrderRequest::alo(ANT, true, 104.0, 0.01, false, None).unwrap();
    assert!(matches!(
        c.order(ANT, &tiny).await,
        Err(ExchangeError::InvalidOrder(_))
    ));
    assert_eq!(seen.lock().unwrap().len(), 3);
}
