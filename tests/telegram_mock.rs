//! Telegram alerter against a loopback mock Bot API (`api_base` override to
//! http://127.0.0.1). Nothing leaves the machine and no real token is used.
//! Checks the request shape, merging of queued alerts, `retry_after` on
//! 429, no retry on 4xx, pacing, queue overflow accounting and flush.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antbot::monitor::telegram::{AlertSettings, Alerter, TelegramConfig};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOKEN: &str = "123456:TEST-not-a-real-token";

#[derive(Debug, Clone)]
struct Req {
    path: String,
    body: Value,
    at: Instant,
}

type Seen = Arc<Mutex<Vec<Req>>>;

const OK: (&str, &str) = ("200 OK", r#"{"ok":true,"result":{"message_id":1}}"#);

/// Answers each POST with the next canned (status, body), then 200 OK.
async fn spawn_mock(replies: Vec<(&'static str, &'static str)>) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
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
                        path,
                        body: serde_json::from_slice(&body).unwrap(),
                        at: Instant::now(),
                    });
                    let (status, payload) = replies.lock().unwrap().next().unwrap_or(OK);
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

fn cfg(base: &str) -> TelegramConfig {
    TelegramConfig {
        token: TOKEN.into(),
        chat_id: "-100777".into(),
        api_base: base.into(),
    }
}

fn settings() -> AlertSettings {
    AlertSettings {
        prefix: "[antbot paper xyz:ANT]".into(),
        queue_capacity: 64,
        max_per_min: 20,
        min_interval: Duration::from_millis(10),
        http_timeout: Duration::from_secs(5),
        // The historical 3 attempts: the tests count requests.
        max_attempts: 3,
        retry_backoff: Duration::from_millis(50),
        max_retry_backoff: Duration::from_secs(1),
    }
}

fn texts(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|r| r.body["text"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivers_in_order_with_prefix_and_chat_id() {
    let (base, seen) = spawn_mock(vec![]).await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    assert!(a.is_enabled());
    a.send("🟢 ENTRY SHORT xyz:ANT");
    a.send("✅ EXIT SHORT xyz:ANT (converged)");
    a.send("🛑 antbot stopping (SIGTERM)");
    assert!(a.flush(Duration::from_secs(10)).await);

    let reqs = seen.lock().unwrap().clone();
    assert!(!reqs.is_empty() && reqs.len() <= 3, "{reqs:?}");
    for r in &reqs {
        assert_eq!(r.path, format!("/bot{TOKEN}/sendMessage"));
        assert_eq!(r.body["chat_id"], "-100777");
        assert_eq!(r.body["disable_web_page_preview"], true);
        assert!(r.body.get("parse_mode").is_none());
        assert!(r.body["text"]
            .as_str()
            .unwrap()
            .starts_with("[antbot paper xyz:ANT]\n"));
    }
    // Every alert delivered once, in order (merged or not).
    let all = texts(&seen).join("\n\n");
    let i1 = all.find("ENTRY SHORT").unwrap();
    let i2 = all.find("EXIT SHORT").unwrap();
    let i3 = all.find("antbot stopping").unwrap();
    assert!(i1 < i2 && i2 < i3);
    assert_eq!(all.matches("ENTRY SHORT").count(), 1);
    assert_eq!(stats.sent.load(Ordering::Relaxed) as usize, reqs.len());
    assert_eq!(stats.failed.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn honours_retry_after_on_429() {
    let (base, seen) = spawn_mock(vec![(
        "429 Too Many Requests",
        r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 1","parameters":{"retry_after":1}}"#,
    )])
    .await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("⛔ KILL SWITCH: test");
    assert!(a.flush(Duration::from_secs(10)).await);
    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].body, reqs[1].body);
    let gap = reqs[1].at.duration_since(reqs[0].at);
    assert!(gap >= Duration::from_millis(950), "retried after {gap:?}");
    assert_eq!(stats.sent.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_5xx_but_not_4xx() {
    let (base, seen) = spawn_mock(vec![
        ("502 Bad Gateway", "oops"),
        OK,
        (
            "400 Bad Request",
            r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
        ),
    ])
    .await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("first");
    assert!(a.flush(Duration::from_secs(10)).await);
    a.send("second");
    assert!(a.flush(Duration::from_secs(10)).await);
    // first: 502 then OK; second: 400, given up without retry.
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert_eq!(stats.sent.load(Ordering::Relaxed), 1);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paces_messages_with_min_interval() {
    let (base, seen) = spawn_mock(vec![]).await;
    let mut s = settings();
    s.min_interval = Duration::from_millis(300);
    let (a, _) = Alerter::spawn(cfg(&base), s);
    a.send("one");
    assert!(a.flush(Duration::from_secs(10)).await);
    a.send("two");
    assert!(a.flush(Duration::from_secs(10)).await);
    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    let gap = reqs[1].at.duration_since(reqs[0].at);
    assert!(gap >= Duration::from_millis(280), "gap {gap:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_queue_drops_without_blocking_and_reports_it() {
    let (base, seen) = spawn_mock(vec![]).await;
    let mut s = settings();
    s.queue_capacity = 1;
    s.min_interval = Duration::from_millis(200);
    let (a, _) = Alerter::spawn(cfg(&base), s);
    let t0 = Instant::now();
    for i in 0..50 {
        a.send(format!("alert {i}"));
    }
    // send() never waits on the network or the pacing.
    assert!(t0.elapsed() < Duration::from_millis(100));
    assert!(a.flush(Duration::from_secs(10)).await);
    a.send("after the burst");
    assert!(a.flush(Duration::from_secs(10)).await);
    let all = texts(&seen).join("\n");
    assert!(all.contains("alerts dropped: queue full"), "{all}");
    assert!(all.contains("after the burst"));
    assert!(all.contains("alert 0"));
}

#[tokio::test]
async fn disabled_alerter_is_a_noop() {
    let a = Alerter::from_env(true, |_| None, AlertSettings::default());
    assert!(!a.is_enabled());
    a.send("nothing");
    assert!(a.flush(Duration::from_millis(10)).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undelivered_alerts_are_reported_in_the_next_message() {
    let (base, seen) = spawn_mock(vec![(
        "400 Bad Request",
        r#"{"ok":false,"error_code":400,"description":"Bad Request: message is too long"}"#,
    )])
    .await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("lost");
    assert!(a.flush(Duration::from_secs(10)).await);
    a.send("next");
    assert!(a.flush(Duration::from_secs(10)).await);
    let t = texts(&seen);
    assert_eq!(t.len(), 2);
    assert_eq!(
        t[1],
        "[antbot paper xyz:ANT]\n(1 earlier alerts could not be delivered)\nnext"
    );
    assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
    assert_eq!(stats.failed_alerts.load(Ordering::Relaxed), 1);
    assert_eq!(stats.sent.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_burst_is_split_without_cutting_alerts() {
    let (base, seen) = spawn_mock(vec![]).await;
    let mut s = settings();
    s.queue_capacity = 256;
    // Slow pacing so the whole burst is queued before the first send.
    s.min_interval = Duration::from_millis(300);
    let (a, _) = Alerter::spawn(cfg(&base), s);
    a.send("warm-up");
    let sent: Vec<String> = (0..60)
        .map(|i| format!("warning {i:02} {}", "w".repeat(120)))
        .chain(std::iter::once(
            "⛔ KILL SWITCH: daily loss limit reached".to_string(),
        ))
        .collect();
    for t in &sent {
        a.send(t.clone());
    }
    assert!(a.flush(Duration::from_secs(20)).await);
    let msgs = texts(&seen);
    assert!(msgs.len() >= 3, "{}", msgs.len());
    for m in &msgs {
        assert!(m.chars().count() <= 3800);
        assert!(!m.contains("(truncated)"));
    }
    let all = msgs.join("\n");
    for t in &sent {
        assert_eq!(all.matches(t.as_str()).count(), 1, "{t}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rides_out_a_longer_5xx_outage_with_more_attempts() {
    // The 2026-11-17 incident: several 502s in a row. Three attempts
    // were not enough; the configured attempts deliver the alert.
    let (base, seen) = spawn_mock(vec![
        ("502 Bad Gateway", "bad gateway"),
        ("502 Bad Gateway", "bad gateway"),
        ("503 Service Unavailable", "unavailable"),
        ("502 Bad Gateway", "bad gateway"),
    ])
    .await;
    let mut s = settings();
    s.max_attempts = 6;
    s.retry_backoff = Duration::from_millis(20);
    s.max_retry_backoff = Duration::from_millis(60);
    let (a, stats) = Alerter::spawn(cfg(&base), s);
    a.send("↩️ exit started xyz:ANT: converged");
    assert!(a.flush(Duration::from_secs(10)).await);
    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs.len(), 5, "4 failures, then delivered");
    assert!(reqs.iter().all(|r| r.body == reqs[0].body));
    // Exponential backoff: 20, 40, 60 (capped), 60 ms (+ jitter).
    let gaps: Vec<Duration> = reqs.windows(2).map(|w| w[1].at - w[0].at).collect();
    for (g, min) in gaps.iter().zip([20u64, 40, 60, 60]) {
        assert!(*g >= Duration::from_millis(min - 5), "{gaps:?}");
    }
    assert_eq!(stats.sent.load(Ordering::Relaxed), 1);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gives_up_after_max_attempts_and_reports_it() {
    let (base, seen) = spawn_mock(vec![("502 Bad Gateway", "x"); 4]).await;
    let mut s = settings();
    s.max_attempts = 4;
    s.retry_backoff = Duration::from_millis(5);
    let (a, stats) = Alerter::spawn(cfg(&base), s);
    a.send("lost");
    assert!(a.flush(Duration::from_secs(10)).await);
    assert_eq!(seen.lock().unwrap().len(), 4);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
    a.send("next");
    assert!(a.flush(Duration::from_secs(10)).await);
    let t = texts(&seen);
    assert_eq!(
        t.last().unwrap(),
        "[antbot paper xyz:ANT]\n(1 earlier alerts could not be delivered)\nnext"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retrying_never_blocks_send() {
    // A mock that never answers: every attempt waits for the HTTP timeout.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let mut keep = Vec::new();
        while let Ok((sock, _)) = l.accept().await {
            keep.push(sock);
        }
    });
    let mut s = settings();
    s.queue_capacity = 4;
    s.http_timeout = Duration::from_millis(300);
    s.max_attempts = 6;
    let (a, stats) = Alerter::spawn(cfg(&base), s);
    a.send("stuck");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t0 = Instant::now();
    for i in 0..20 {
        a.send(format!("alert {i}"));
    }
    // The worker is busy retrying: send() still returns at once, and the
    // overflow is dropped and counted exactly as before.
    assert!(
        t0.elapsed() < Duration::from_millis(50),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(stats.dropped.load(Ordering::Relaxed), 16);
    // A bounded flush returns on time even though nothing can be sent.
    let t1 = Instant::now();
    assert!(!a.flush(Duration::from_millis(200)).await);
    assert!(t1.elapsed() < Duration::from_millis(500));
}

fn closed_trade() -> antbot::strategy::signal::TradeRecord {
    use antbot::strategy::premium::Side;
    use antbot::strategy::signal::{ExitReason, TradeRecord};
    TradeRecord {
        side: Side::Short,
        size: 20.19 + 0.35 + 0.02,
        entry_px: 103.11,
        exit_px: 102.6,
        signal_ms: 0,
        opened_ms: 1_000,
        closed_ms: 2_701_000,
        entry_premium_bps: Some(62.3),
        exit_premium_bps: Some(8.0),
        fees_usd: 0.13,
        funding_usd: 0.05,
        gross_pnl_usd: 10.5,
        pnl_usd: 10.42,
        pnl_bps: 49.2,
        exit_reason: ExitReason::Converged,
        exit_taker_sz: 0.0,
    }
}

fn monitor(a: &Alerter) -> antbot::monitor::Monitor {
    antbot::monitor::Monitor::new(
        antbot::monitor::csv::Journal::default(),
        a.clone(),
        "xyz:ANT",
    )
    .with_sz_decimals(2)
    .with_exit_coalesce_ms(10_000)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_started_and_trade_closed_within_the_window_are_one_message() {
    use antbot::strategy::signal::ExitReason;
    let (base, seen) = spawn_mock(vec![]).await;
    let (a, _) = Alerter::spawn(cfg(&base), settings());
    let mut m = monitor(&a);
    let t0 = 1_800_000_000_000;
    m.exit_started(t0, ExitReason::Converged, Some(8.3));
    // Nothing goes out while the start is held.
    m.flush_due(t0 + 2_000);
    assert!(a.flush(Duration::from_secs(5)).await);
    assert!(texts(&seen).is_empty());
    m.trade_closed(t0 + 2_500, &closed_trade());
    m.flush_due(t0 + 60_000);
    drop(m);
    assert!(a.flush(Duration::from_secs(5)).await);
    let t = texts(&seen);
    assert_eq!(t.len(), 1, "{t:?}");
    assert!(
        t[0].contains("✅ EXIT SHORT xyz:ANT (converged)\n20.56 103.1100"),
        "{}",
        t[0]
    );
    assert!(
        t[0].ends_with("\n↩️ exit started 2s before: converged, premium +8.3 bps"),
        "{}",
        t[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_started_without_close_is_flushed_after_the_window_or_on_drop() {
    use antbot::strategy::signal::ExitReason;
    let (base, seen) = spawn_mock(vec![]).await;
    let (a, _) = Alerter::spawn(cfg(&base), settings());
    let mut m = monitor(&a);
    let t0 = 1_800_000_000_000;
    // Outside the window: two messages.
    m.exit_started(t0, ExitReason::MaxHold, None);
    m.flush_due(t0 + 10_000);
    assert!(a.flush(Duration::from_secs(5)).await);
    m.trade_closed(t0 + 900_000, &closed_trade());
    assert!(a.flush(Duration::from_secs(5)).await);
    let t = texts(&seen);
    assert_eq!(t.len(), 2, "{t:?}");
    assert!(t[0].ends_with("↩️ exit started xyz:ANT: max_hold\npremium n/a"));
    assert!(t[1].contains("✅ EXIT SHORT") && !t[1].contains("exit started"));
    // Held at the end of the run (no close, no flush): queued on drop.
    m.exit_started(t0 + 1_000_000, ExitReason::Kill, None);
    drop(m);
    assert!(a.flush(Duration::from_secs(5)).await);
    let t = texts(&seen);
    assert_eq!(t.len(), 3, "{t:?}");
    assert!(t[2].ends_with("↩️ exit started xyz:ANT: kill\npremium n/a"));
}
