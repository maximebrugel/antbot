//! The outbound alert worker never writes the bot token to the logs, even
//! when the Bot API (or a proxy in front of it) echoes the request URL in
//! an error `description`, and a damaged `200` answer is not counted as a
//! delivery. Loopback mock only: nothing leaves the machine and no real
//! token is used.
//!
//! Logs are captured with the same two sinks as the bot (a stdout-like
//! writer and the daily rolling file of `monitor::logging`) at the default
//! `info` filter. The tests use a current-thread runtime so the worker task
//! runs on the thread holding the capturing subscriber.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antbot::monitor::logging::{file_appender, DEFAULT_FILTER};
use antbot::monitor::telegram::{AlertSettings, Alerter, TelegramConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, EnvFilter};

const TOKEN: &str = "987654:FAKE-redaction-probe-token";

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Captured "stdout" and the rolling log directory.
struct Capture {
    stdout: Buf,
    dir: tempfile::TempDir,
    _guard: tracing::subscriber::DefaultGuard,
}

impl Capture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let stdout = Buf::default();
        let out = stdout.clone();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new(DEFAULT_FILTER))
            .with(
                fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || out.clone()),
            )
            .with(
                fmt::layer()
                    .with_ansi(false)
                    .with_writer(file_appender(dir.path(), 30).unwrap()),
            );
        let guard = tracing::subscriber::set_default(subscriber);
        Self {
            stdout,
            dir,
            _guard: guard,
        }
    }

    /// (stdout, every rolling log file concatenated).
    fn text(&self) -> (String, String) {
        let stdout = String::from_utf8_lossy(&self.stdout.0.lock().unwrap()).to_string();
        let mut file = String::new();
        for e in std::fs::read_dir(self.dir.path().join("logs")).unwrap() {
            file.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
        }
        (stdout, file)
    }

    /// Neither sink holds the token (nor a cut-off head of it), and both
    /// hold `expect` (so the check is not vacuous).
    fn assert_clean(&self, expect: &str) {
        let (stdout, file) = self.text();
        let head = &TOKEN[..10];
        for (name, s) in [("stdout", &stdout), ("log file", &file)] {
            assert!(!s.contains(TOKEN), "token in {name}:\n{s}");
            assert!(!s.contains(head), "token head in {name}:\n{s}");
            assert!(s.contains(expect), "{name} lacks {expect:?}:\n{s}");
        }
    }
}

/// One raw HTTP answer per connection (then the connection is closed);
/// `200 OK` once the list is exhausted. Returns the base URL and the
/// request count.
async fn spawn_raw_mock(replies: Vec<String>) -> (String, Arc<Mutex<usize>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let count = Arc::new(Mutex::new(0usize));
    let c2 = count.clone();
    let replies = Arc::new(Mutex::new(replies.into_iter()));
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = l.accept().await.unwrap();
            let count = c2.clone();
            let replies = replies.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
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
                            break;
                        }
                    }
                }
                *count.lock().unwrap() += 1;
                let reply = replies
                    .lock()
                    .unwrap()
                    .next()
                    .unwrap_or_else(|| json("200 OK", r#"{"ok":true,"result":{}}"#));
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (base, count)
}

/// A complete JSON answer.
fn json(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// An answer whose `description` echoes the request URL with the token.
fn echo(status: &str, code: u16, extra: &str) -> String {
    json(
        status,
        &format!(
            r#"{{"ok":false,"error_code":{code},"description":"upstream URL https://api.telegram.org/bot{TOKEN}/sendMessage failed"{extra}}}"#
        ),
    )
}

fn cfg(base: &str) -> TelegramConfig {
    TelegramConfig {
        token: TOKEN.into(),
        chat_id: "424242".into(),
        api_base: base.into(),
    }
}

fn settings() -> AlertSettings {
    AlertSettings {
        prefix: "[antbot paper xyz:ANT]".into(),
        queue_capacity: 16,
        max_per_min: 60,
        min_interval: Duration::from_millis(1),
        http_timeout: Duration::from_secs(5),
        // The historical 3 attempts: the tests count requests.
        max_attempts: 3,
        retry_backoff: Duration::from_millis(20),
        max_retry_backoff: Duration::from_secs(1),
    }
}

#[tokio::test]
async fn token_echoed_in_4xx_description_is_redacted() {
    let cap = Capture::new();
    let (base, count) = spawn_raw_mock(vec![echo("401 Unauthorized", 401, "")]).await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("antbot started");
    assert!(a.flush(Duration::from_secs(10)).await);
    assert_eq!(*count.lock().unwrap(), 1, "4xx is not retried");
    assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
    cap.assert_clean("bot<redacted>/sendMessage");
}

#[tokio::test]
async fn token_echoed_in_5xx_and_429_descriptions_is_redacted() {
    let cap = Capture::new();
    let (base, count) = spawn_raw_mock(vec![
        echo("500 Internal Server Error", 500, ""),
        echo(
            "429 Too Many Requests",
            429,
            r#","parameters":{"retry_after":0}"#,
        ),
        echo("502 Bad Gateway", 502, ""),
    ])
    .await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("kill switch");
    assert!(a.flush(Duration::from_secs(10)).await);
    // Every attempt failed and was logged (two retries, then given up).
    assert_eq!(*count.lock().unwrap(), 3);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
    let (stdout, _) = cap.text();
    assert_eq!(stdout.matches("bot<redacted>/sendMessage").count(), 3);
    cap.assert_clean("telegram alert not delivered");
}

#[tokio::test]
async fn token_straddling_the_description_cut_is_redacted() {
    let cap = Capture::new();
    // Without redaction before the cut, the first 10 chars of the token
    // would survive at the end of the 200-char description.
    let desc = format!("{}{TOKEN}", "x".repeat(190));
    let body = format!(r#"{{"ok":false,"error_code":403,"description":"{desc}"}}"#);
    let (base, _) = spawn_raw_mock(vec![json("403 Forbidden", &body)]).await;
    let (a, _) = Alerter::spawn(cfg(&base), settings());
    a.send("x");
    assert!(a.flush(Duration::from_secs(10)).await);
    cap.assert_clean(&format!("{}<redacted>", "x".repeat(190)));
}

#[tokio::test]
async fn transport_errors_never_log_the_token() {
    let cap = Capture::new();
    // A port nobody listens on: connection refused on every attempt.
    let base = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("x");
    assert!(a.flush(Duration::from_secs(10)).await);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
    cap.assert_clean("telegram send failed, retrying");
}

#[tokio::test]
async fn truncated_200_body_is_retried_not_counted_as_sent() {
    let cap = Capture::new();
    // Content-Length 100, but the connection closes after one byte.
    let cut =
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: 100\r\n\r\n{"
            .to_string();
    let (base, count) = spawn_raw_mock(vec![cut]).await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("x");
    assert!(a.flush(Duration::from_secs(10)).await);
    assert_eq!(*count.lock().unwrap(), 2, "retried once, then delivered");
    assert_eq!(stats.sent.load(Ordering::Relaxed), 1);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 0);
    cap.assert_clean("reading body");
}

#[tokio::test]
async fn malformed_200_body_is_retried_and_counted_as_failed() {
    let cap = Capture::new();
    let bad = || json("200 OK", "<html>gateway</html>");
    let (base, count) = spawn_raw_mock(vec![bad(), bad(), bad()]).await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("x");
    assert!(a.flush(Duration::from_secs(10)).await);
    assert_eq!(*count.lock().unwrap(), 3);
    assert_eq!(stats.sent.load(Ordering::Relaxed), 0);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
    cap.assert_clean("malformed response body");
}

#[tokio::test]
async fn ok_false_200_is_not_counted_as_sent() {
    let cap = Capture::new();
    let (base, count) = spawn_raw_mock(vec![json(
        "200 OK",
        r#"{"ok":false,"description":"rejected"}"#,
    )])
    .await;
    let (a, stats) = Alerter::spawn(cfg(&base), settings());
    a.send("x");
    assert!(a.flush(Duration::from_secs(10)).await);
    assert_eq!(*count.lock().unwrap(), 1);
    assert_eq!(stats.sent.load(Ordering::Relaxed), 0);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
    cap.assert_clean("rejected");
}
