//! Telegram command task against a loopback mock Bot API (`api_base`
//! override to http://127.0.0.1). Nothing leaves the machine and no real
//! token is used. Checks the getUpdates / sendMessage wire format, offset
//! confirmation, the authorization filter end to end, recovery from a 409
//! conflict and malformed answers, and that the token never shows up in
//! errors.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antbot::monitor::commands::{
    BotApi, CommandSettings, CommandsHandle, HttpBotApi, StatusSnapshot,
};
use antbot::monitor::telegram::TelegramConfig;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

const TOKEN: &str = "123456:TEST-not-a-real-token";
const ME: i64 = 777_001;

#[derive(Debug, Clone)]
struct Req {
    path: String,
    body: Value,
}

type Seen = Arc<Mutex<Vec<Req>>>;
type Script = Arc<Mutex<VecDeque<(&'static str, String)>>>;

/// getUpdates answers come from `script` (then an empty result after a
/// short "long poll"); sendMessage always succeeds.
async fn spawn_mock(script: Vec<(&'static str, String)>) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let s2 = seen.clone();
    let script: Script = Arc::new(Mutex::new(script.into()));
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = l.accept().await.unwrap();
            let seen = s2.clone();
            let script = script.clone();
            tokio::spawn(async move {
                loop {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    let (path, body) = loop {
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
                                let path = s
                                    .lines()
                                    .next()
                                    .unwrap()
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap()
                                    .to_string();
                                break (path, buf[h + 4..h + 4 + len].to_vec());
                            }
                        }
                    };
                    seen.lock().unwrap().push(Req {
                        path: path.clone(),
                        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                    });
                    let (status, payload) =
                        if path.ends_with("/getUpdates") && body_offset(&seen) == Some(-1) {
                            // Startup backlog skip: one old command.
                            let old = update(3, ME, "private", ME, "/pnl", now_s() - 7200);
                            ok(vec![old])
                        } else if path.ends_with("/getUpdates") {
                            let next = script.lock().unwrap().pop_front();
                            match next {
                                Some(r) => r,
                                None => {
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                    ("200 OK", r#"{"ok":true,"result":[]}"#.to_string())
                                }
                            }
                        } else {
                            (
                                "200 OK",
                                r#"{"ok":true,"result":{"message_id":1}}"#.to_string(),
                            )
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
    (base, seen)
}

/// `offset` of the last recorded request.
fn body_offset(seen: &Seen) -> Option<i64> {
    seen.lock()
        .unwrap()
        .last()
        .and_then(|r| r.body["offset"].as_i64())
}

fn cfg(base: &str) -> TelegramConfig {
    TelegramConfig {
        token: TOKEN.into(),
        chat_id: ME.to_string(),
        api_base: base.into(),
    }
}

fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn update(id: i64, chat: i64, chat_type: &str, from: i64, text: &str, date: i64) -> Value {
    json!({
        "update_id": id,
        "message": {
            "message_id": id,
            "date": date,
            "chat": {"id": chat, "type": chat_type},
            "from": {"id": from, "is_bot": false, "first_name": "op"},
            "text": text,
        }
    })
}

fn ok(updates: Vec<Value>) -> (&'static str, String) {
    ("200 OK", json!({"ok": true, "result": updates}).to_string())
}

fn settings() -> CommandSettings {
    let mut s = CommandSettings::new(ME, now_s());
    s.long_poll_secs = 1;
    s.backoff_initial = Duration::from_millis(50);
    s.backoff_max = Duration::from_millis(200);
    s.reply_gap = Duration::from_millis(10);
    s
}

fn snapshot() -> watch::Receiver<Arc<StatusSnapshot>> {
    let s = StatusSnapshot {
        prefix: "[antbot paper xyz:ANT]".into(),
        as_of_ms: 1_800_000_000_000,
        phase: "flat".into(),
        realized_total_usd: 12.5,
        trades_total: 3,
        ..StatusSnapshot::default()
    };
    let (tx, rx) = watch::channel(Arc::new(s));
    // Keep the sender alive for the whole test.
    std::mem::forget(tx);
    rx
}

async fn wait_for(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition not reached");
}

fn sends(seen: &Seen) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r.path.ends_with("/sendMessage"))
        .map(|r| r.body.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_only_the_authorized_private_chat() {
    let t = now_s();
    let (base, seen) = spawn_mock(vec![
        // 409 conflict first (another consumer), then garbage, then data.
        (
            "409 Conflict",
            r#"{"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request"}"#.into(),
        ),
        ("200 OK", "not json".into()),
        ok(vec![
            update(5, ME, "private", ME, "/PNL@antbot_bot now", t),
            update(6, ME + 1, "private", ME + 1, "/pnl", t),
            update(7, -1001, "supergroup", ME, "/pnl", t),
            update(8, ME, "private", ME, "/pnl", t - 3600),
            json!({"update_id": 9, "edited_message": update(9, ME, "private", ME, "/pnl", t)["message"]}),
            update(10, ME, "private", ME, "/trades", t),
            update(11, ME, "private", ME, "what?", t),
        ]),
    ])
    .await;
    let api = HttpBotApi::new(&cfg(&base), 1).unwrap();
    let h = CommandsHandle::spawn(api, settings(), snapshot());
    wait_for(|| sends(&seen).len() >= 3).await;
    // The next poll confirms everything up to update 11.
    wait_for(|| h.offset() == 12).await;
    wait_for(|| {
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| r.path.ends_with("/getUpdates") && r.body["offset"] == 12)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let out = sends(&seen);
    assert_eq!(out.len(), 3, "{out:?}");
    for b in &out {
        assert_eq!(b["chat_id"], ME.to_string());
        assert!(b.get("parse_mode").is_none(), "plain text only");
    }
    assert!(out[0]["text"]
        .as_str()
        .unwrap()
        .contains("realized PnL: +12.50 USD"));
    assert!(out[1]["text"]
        .as_str()
        .unwrap()
        .contains("no closed trades yet"));
    assert!(out[2]["text"]
        .as_str()
        .unwrap()
        .contains("Read-only commands"));

    let reqs = seen.lock().unwrap().clone();
    let polls: Vec<&Req> = reqs
        .iter()
        .filter(|r| r.path.ends_with("/getUpdates"))
        .collect();
    assert_eq!(polls[0].path, format!("/bot{TOKEN}/getUpdates"));
    // Backlog skip first (the old /pnl, update 3, is never answered)...
    assert_eq!(polls[0].body["offset"], -1);
    assert_eq!(polls[0].body["timeout"], 0);
    // ... then long polls from after it.
    assert_eq!(polls[1].body["offset"], 4);
    assert_eq!(polls[1].body["timeout"], 1);
    assert_eq!(polls[1].body["allowed_updates"], json!(["message"]));
    assert!(h.stats.poll_errors.load(Ordering::Relaxed) >= 2);
    assert_eq!(h.stats.handled.load(Ordering::Relaxed), 3);
    assert_eq!(h.stats.ignored.load(Ordering::Relaxed), 5);
    h.shutdown(Duration::from_secs(2)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_never_contain_the_token() {
    // Connection refused: the reqwest error would name the URL.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let api = HttpBotApi::new(&cfg(&base), 1).unwrap();
    let e = api.get_updates(0, 1).await.unwrap_err();
    assert!(!e.message.contains(TOKEN), "{e}");
    assert!(!e.message.contains("123456"), "{e}");
    let e = api.send_message("x".into()).await.unwrap_err();
    assert!(!e.message.contains(TOKEN), "{e}");

    // An API answer echoing the token (defence in depth).
    let (base, _seen) = spawn_mock(vec![(
        "401 Unauthorized",
        json!({"ok": false, "error_code": 401, "description": format!("Unauthorized bot{TOKEN}")})
            .to_string(),
    )])
    .await;
    let api = HttpBotApi::new(&cfg(&base), 1).unwrap();
    let e = api.get_updates(0, 1).await.unwrap_err();
    assert!(!e.message.contains(TOKEN), "{e}");
    assert!(e.message.contains("401"), "{e}");
    assert!(e.message.contains("<redacted>"), "{e}");

    // 429 carries retry_after.
    let (base, _seen) = spawn_mock(vec![(
        "429 Too Many Requests",
        r#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":3}}"#.into(),
    )])
    .await;
    let api = HttpBotApi::new(&cfg(&base), 1).unwrap();
    let e = api.get_updates(0, 1).await.unwrap_err();
    assert_eq!(e.retry_after, Some(Duration::from_secs(3)));
}
