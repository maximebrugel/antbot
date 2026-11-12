//! Offline feed tests: a local websocket server (tokio-tungstenite) and a
//! minimal local HTTP server stand in for Hyperliquid. Exercises subscription,
//! reconnect + resubscribe, stale detection, REST fallback and REST retries.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use antbot::config::Config;
use antbot::hl::info::{InfoClient, RetryPolicy, TokenBucket};
use antbot::hl::market::DataSource;
use antbot::hl::ws::{FeedConfig, MarketFeed, UserEvent};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/",
            $name
        ))
    };
}

/// Minimal HTTP/1.1 server answering POST /info by request type. The first
/// `fail_first` requests get a 500 (retry test).
async fn spawn_http(fail_first: usize) -> (String, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/info", l.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = count.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = l.accept().await.unwrap();
            let count = c2.clone();
            tokio::spawn(async move {
                loop {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    // read headers + body (Content-Length)
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
                                    let l = l.to_ascii_lowercase();
                                    l.strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if buf.len() >= h + 4 + len {
                                break String::from_utf8_lossy(&buf[h + 4..h + 4 + len])
                                    .to_string();
                            }
                        }
                    };
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    let (status, payload) = if n < fail_first {
                        ("500 Internal Server Error", "oops".to_string())
                    } else if body.contains("metaAndAssetCtxs") {
                        ("200 OK", fixture!("metaAndAssetCtxs_xyz.json").to_string())
                    } else if body.contains("l2Book") {
                        ("200 OK", fixture!("l2Book_ANT.json").to_string())
                    } else if body.contains("perpDexs") {
                        ("200 OK", fixture!("perpDexs.json").to_string())
                    } else {
                        ("400 Bad Request", "unknown".to_string())
                    };
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
    (url, count)
}

fn info(url: &str) -> InfoClient {
    InfoClient::new(
        url,
        Duration::from_secs(5),
        Arc::new(TokenBucket::new(1000.0, 1000.0)),
    )
    .unwrap()
    .with_retry(RetryPolicy {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(50),
    })
}

fn feed_cfg(ws_url: &str, user: Option<String>) -> FeedConfig {
    let mut c = FeedConfig::from_config(&Config::default(), ws_url, user);
    c.ping_interval = Duration::from_millis(200);
    c.stale_after = Duration::from_millis(800);
    c.rest_poll_interval = Duration::from_millis(100);
    c.backoff_initial = Duration::from_millis(50);
    c.backoff_max = Duration::from_millis(200);
    c
}

async fn wait_for<F: Fn(&antbot::hl::market::MarketState) -> bool>(feed: &MarketFeed, f: F) {
    let mut rx = feed.state.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if f(&rx.borrow_and_update()) {
                return;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("condition not reached in time");
}

#[tokio::test]
async fn info_client_retries_5xx() {
    let (url, count) = spawn_http(2).await;
    let c = info(&url);
    let pd = c.perp_dexs().await.unwrap();
    assert_eq!(pd[1].as_ref().unwrap().name, "xyz");
    assert_eq!(count.load(Ordering::SeqCst), 3);
    // 4xx is not retried
    let before = count.load(Ordering::SeqCst);
    assert!(c.user_fills("0x0").await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), before + 1);
}

/// WS server: connection 1 sends book + ctx then goes silent (-> stale ->
/// reconnect); connection 2 gets the resubscription and sends data again.
#[tokio::test]
async fn ws_subscribe_stale_reconnect_resubscribe() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", l.local_addr().unwrap());
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<(usize, String)>();
    tokio::spawn(async move {
        for conn in 0..2usize {
            let (sock, _) = l.accept().await.unwrap();
            let sub_tx = sub_tx.clone();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
                let mut subs = 0;
                while let Some(Ok(m)) = ws.next().await {
                    if let Message::Text(t) = m {
                        let t = t.to_string();
                        if t.contains("subscribe") {
                            subs += 1;
                            sub_tx.send((conn, t)).unwrap();
                            if subs == 4 {
                                ws.send(Message::text(fixture!("ws_l2Book.json")))
                                    .await
                                    .unwrap();
                                ws.send(Message::text(fixture!("ws_activeAssetCtx.json")))
                                    .await
                                    .unwrap();
                                let fills = format!(
                                    r#"{{"channel":"userFills","data":{{"isSnapshot":true,"user":"0xabc","fills":{}}}}}"#,
                                    fixture!("userFills_sample.json")
                                );
                                ws.send(Message::text(fills)).await.unwrap();
                            }
                        } else if t.contains("ping") {
                            // conn 0: never answer, never send data -> stale
                            if conn == 1 {
                                ws.send(Message::text(r#"{"channel":"pong"}"#))
                                    .await
                                    .unwrap();
                                ws.send(Message::text(fixture!("ws_activeAssetCtx.json")))
                                    .await
                                    .unwrap();
                            }
                        }
                    }
                }
            });
        }
    });

    // HTTP server for the fallback (WS stale window).
    let (http_url, _) = spawn_http(0).await;
    let mut feed = MarketFeed::spawn(feed_cfg(&ws_url, Some("0xabc".into())), info(&http_url));

    // 4 subscriptions on the first connection.
    let mut first = vec![];
    for _ in 0..4 {
        first.push(sub_rx.recv().await.unwrap());
    }
    assert!(first.iter().all(|(c, _)| *c == 0));
    assert!(first[0].1.contains(r#""type":"l2Book""#));
    assert!(first[1].1.contains(r#""type":"activeAssetCtx""#));
    assert!(first[2].1.contains(r#""type":"userFills""#));
    assert!(first[3].1.contains(r#""type":"orderUpdates""#));

    wait_for(&feed, |s| {
        s.ctx_source == DataSource::Ws && s.best_bid.is_some()
    })
    .await;
    let s = feed.snapshot();
    assert_eq!(s.oracle_px, Some(104.57));
    assert!(s.ws_connected);

    match tokio::time::timeout(Duration::from_secs(5), feed.user_events.recv())
        .await
        .unwrap()
        .unwrap()
    {
        UserEvent::Fills { is_snapshot, fills } => {
            assert!(is_snapshot);
            assert_eq!(fills.len(), 3);
        }
        e => panic!("{e:?}"),
    }

    // Connection 0 goes silent -> stale -> REST fallback kicks in.
    wait_for(&feed, |s| {
        s.rest_polls > 0 && s.ctx_source == DataSource::Rest
    })
    .await;

    // Reconnect + resubscribe on connection 1.
    let mut second = vec![];
    for _ in 0..4 {
        second.push(sub_rx.recv().await.unwrap());
    }
    assert!(second.iter().all(|(c, _)| *c == 1));
    wait_for(&feed, |s| {
        s.ws_connects >= 2 && s.ws_connected && s.ctx_source == DataSource::Ws
    })
    .await;

    feed.shutdown().await;
}

#[tokio::test]
async fn rest_only_mode_polls() {
    let (http_url, count) = spawn_http(0).await;
    let mut cfg = feed_cfg("ws://127.0.0.1:1", None);
    cfg.ws_enabled = false;
    let feed = MarketFeed::spawn(cfg, info(&http_url));
    wait_for(&feed, |s| s.rest_polls >= 2).await;
    let s = feed.snapshot();
    assert_eq!(s.ctx_source, DataSource::Rest);
    assert_eq!(s.book_source, DataSource::Rest);
    assert_eq!(s.best_bid.unwrap().px, 104.24);
    assert_eq!(s.impact_bid_px, Some(104.2221));
    assert!(!s.ws_connected);
    assert!(count.load(Ordering::SeqCst) >= 4);
    feed.shutdown().await;
}
