//! Internal health / metrics HTTP server and the `antbot healthcheck` probe.
//!
//! Enabled by `[monitor].health_listen` (or `ANTBOT_HEALTH_LISTEN`), which
//! must be a loopback `ip:port` (validated by the config): the server is
//! for the container's own healthcheck and a local scraper / sidecar,
//! never for the outside world. It is a deliberately tiny HTTP/1.1
//! implementation on `tokio::net::TcpListener` (one request per
//! connection, `Connection: close`, request head capped at 8 KiB, 5 s per
//! connection, at most 16 connections at once): no extra dependency.
//!
//! The main loop publishes a [`HealthSnapshot`] through a
//! `tokio::sync::watch` channel every second (and after entries, exits,
//! kills and reconciliations); the server only reads the latest one.
//!
//! Endpoints (`GET`, `HEAD` also accepted):
//! * `/healthz`: JSON (snapshot + `healthy` + `problems`), `200 OK` when
//!   healthy, `503 Service Unavailable` otherwise. Healthy = a snapshot was
//!   published recently, the websocket is not stale, the market data is
//!   younger than the stale threshold, reconciliation works, no position
//!   mismatch and no kill switch.
//! * `/metrics`: Prometheus text exposition (`text/plain; version=0.0.4`);
//!   `/metrics?format=json` returns the same values as JSON.
//!
//! `antbot healthcheck` ([`probe`]) queries `/healthz` and exits 0 only on
//! `200`; without a health server it falls back to the run-lock heartbeat
//! ([`check_lock`]). It needs no shell tool (`curl`, `wget`) and writes
//! nothing, so it works in a read-only `debian:*-slim` container.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;

/// Longest accepted request head.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// Budget for one connection (read request + write response).
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
/// Concurrent connections served; extra ones are closed at once.
pub const MAX_CONNECTIONS: usize = 16;
/// A snapshot older than this (or twice the stale threshold, if larger)
/// means the main loop is stuck.
pub const MIN_SNAPSHOT_MAX_AGE_MS: u64 = 30_000;
/// Prometheus text exposition content type.
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
const JSON_CONTENT_TYPE: &str = "application/json";

/// Live bot status published by the main loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HealthSnapshot {
    /// Process start (ms since the epoch).
    pub started_at_ms: u64,
    /// Last publication by the main loop (0 = none yet).
    pub updated_at_ms: u64,
    /// `[monitor].instance_name` (or `antbot`).
    pub instance: String,
    pub mode: String,
    pub coin: String,
    /// Strategy phase (`flat`, `entering`, `in_position`, ...).
    pub phase: String,
    pub ws_connected: bool,
    pub ws_stale: bool,
    /// Age of the freshest market data (book / ctx); `None` = none yet.
    pub data_age_ms: Option<u64>,
    /// `[timing].ws_stale_secs` in ms: older data is unhealthy.
    pub stale_threshold_ms: u64,
    /// The last reconciliation read the account (always true in paper).
    pub reconcile_ok: bool,
    /// Consecutive failed reconciliation account reads.
    pub reconcile_failures: u32,
    /// Bot and exchange disagree on the position (live modes).
    pub position_mismatch: bool,
    pub killed: bool,
    /// Kill rule (`hit_rate`, `persistent_premium`, `manual`, ...).
    pub kill_rule: Option<String>,
    pub equity_usd: f64,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    /// Signed position size (> 0 long), exchange view when known.
    pub position_size: f64,
    pub open_orders: usize,
    pub trades_closed: usize,
    /// Signal premium (rolling mean once warm).
    pub premium_bps: Option<f64>,
    /// Instantaneous premium.
    pub raw_premium_bps: Option<f64>,
    /// The rolling-mean window is covered enough to trade.
    pub premium_ready: bool,
}

impl HealthSnapshot {
    /// Why the bot is not healthy (empty = healthy).
    pub fn problems(&self, now_ms: u64) -> Vec<String> {
        let mut out = Vec::new();
        if self.updated_at_ms == 0 {
            out.push("starting: no status published yet".to_string());
            return out;
        }
        let max_age = self
            .stale_threshold_ms
            .saturating_mul(2)
            .max(MIN_SNAPSHOT_MAX_AGE_MS);
        let age = now_ms.saturating_sub(self.updated_at_ms);
        if age > max_age {
            out.push(format!(
                "main loop stalled: last status {age} ms ago (> {max_age} ms)"
            ));
        }
        if self.ws_stale {
            out.push("websocket market data stale".to_string());
        }
        match self.data_age_ms {
            None => out.push("no market data yet".to_string()),
            Some(a) if a > self.stale_threshold_ms => out.push(format!(
                "market data {a} ms old (> {} ms)",
                self.stale_threshold_ms
            )),
            Some(_) => {}
        }
        if !self.reconcile_ok {
            out.push(format!(
                "reconciliation failing ({} consecutive failures)",
                self.reconcile_failures
            ));
        }
        if self.position_mismatch {
            out.push("position mismatch with the exchange".to_string());
        }
        if self.killed {
            out.push(format!(
                "kill switch active ({})",
                self.kill_rule.as_deref().unwrap_or("unknown rule")
            ));
        }
        out
    }

    pub fn is_healthy(&self, now_ms: u64) -> bool {
        self.problems(now_ms).is_empty()
    }
}

/// `/healthz` and `/metrics?format=json` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthReport {
    pub status: String,
    pub healthy: bool,
    pub problems: Vec<String>,
    pub now_ms: u64,
    #[serde(flatten)]
    pub snapshot: HealthSnapshot,
}

impl HealthReport {
    pub fn new(snapshot: &HealthSnapshot, now_ms: u64) -> Self {
        let problems = snapshot.problems(now_ms);
        let healthy = problems.is_empty();
        Self {
            status: if healthy { "ok" } else { "unhealthy" }.to_string(),
            healthy,
            problems,
            now_ms,
            snapshot: snapshot.clone(),
        }
    }
}

/// A response before serialization to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl HttpResponse {
    fn json<T: Serialize>(status: u16, v: &T) -> Self {
        Self {
            status,
            content_type: JSON_CONTENT_TYPE,
            body: serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into()) + "\n",
        }
    }

    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: format!("{body}\n"),
        }
    }

    /// Full HTTP/1.1 response bytes (`HEAD`: headers only).
    pub fn to_bytes(&self, head_only: bool) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            self.status,
            reason_phrase(self.status),
            self.content_type,
            self.body.len()
        )
        .into_bytes();
        if !head_only {
            out.extend_from_slice(self.body.as_bytes());
        }
        out
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Route one request (pure; the server and the tests call it).
pub fn route(method: &str, target: &str, snap: &HealthSnapshot, now_ms: u64) -> HttpResponse {
    if method != "GET" && method != "HEAD" {
        return HttpResponse::text(405, "method not allowed (GET only)");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let json_wanted = query
        .split('&')
        .any(|kv| kv.eq_ignore_ascii_case("format=json"));
    match path {
        "/healthz" => {
            let r = HealthReport::new(snap, now_ms);
            HttpResponse::json(if r.healthy { 200 } else { 503 }, &r)
        }
        "/metrics" if json_wanted => HttpResponse::json(200, &HealthReport::new(snap, now_ms)),
        "/metrics" => HttpResponse {
            status: 200,
            content_type: PROMETHEUS_CONTENT_TYPE,
            body: prometheus_text(snap, now_ms),
        },
        _ => HttpResponse::text(404, "not found (try /healthz or /metrics)"),
    }
}

fn prom_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else {
        format!("{v}")
    }
}

fn prom_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn b(v: bool) -> f64 {
    if v {
        1.0
    } else {
        0.0
    }
}

/// Prometheus text exposition (format 0.0.4) of a snapshot.
pub fn prometheus_text(s: &HealthSnapshot, now_ms: u64) -> String {
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, labels: &str, v: f64| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        let _ = writeln!(out, "{name}{labels} {}", prom_value(v));
    };
    let opt = |v: Option<f64>| v.unwrap_or(f64::NAN);
    gauge(
        "antbot_up",
        "The antbot process is serving metrics.",
        "",
        1.0,
    );
    gauge(
        "antbot_info",
        "Instance identity (value is always 1).",
        &format!(
            "{{instance=\"{}\",mode=\"{}\",coin=\"{}\"}}",
            prom_label(&s.instance),
            prom_label(&s.mode),
            prom_label(&s.coin)
        ),
        1.0,
    );
    gauge(
        "antbot_healthy",
        "1 when /healthz answers 200.",
        "",
        b(s.is_healthy(now_ms)),
    );
    gauge(
        "antbot_started_at_seconds",
        "Process start time (Unix seconds).",
        "",
        s.started_at_ms as f64 / 1000.0,
    );
    gauge(
        "antbot_status_age_ms",
        "Age of the status published by the main loop.",
        "",
        if s.updated_at_ms == 0 {
            f64::NAN
        } else {
            now_ms.saturating_sub(s.updated_at_ms) as f64
        },
    );
    gauge(
        "antbot_ws_connected",
        "Websocket connected.",
        "",
        b(s.ws_connected),
    );
    gauge(
        "antbot_ws_stale",
        "Websocket market data stale.",
        "",
        b(s.ws_stale),
    );
    gauge(
        "antbot_data_age_ms",
        "Age of the freshest market data (NaN = none yet).",
        "",
        opt(s.data_age_ms.map(|a| a as f64)),
    );
    gauge(
        "antbot_phase_info",
        "Current strategy phase (value is always 1).",
        &format!("{{phase=\"{}\"}}", prom_label(&s.phase)),
        1.0,
    );
    gauge("antbot_killed", "Kill switch active.", "", b(s.killed));
    gauge(
        "antbot_position_mismatch",
        "Bot and exchange disagree on the position.",
        "",
        b(s.position_mismatch),
    );
    gauge(
        "antbot_reconcile_ok",
        "Last reconciliation read the account.",
        "",
        b(s.reconcile_ok),
    );
    gauge(
        "antbot_reconcile_failures",
        "Consecutive failed reconciliation account reads.",
        "",
        s.reconcile_failures as f64,
    );
    gauge(
        "antbot_equity_usd",
        "Strategy equity (realized + unrealized), USD.",
        "",
        s.equity_usd,
    );
    gauge(
        "antbot_realized_pnl_usd",
        "Realized PnL of closed trades, USD.",
        "",
        s.realized_pnl_usd,
    );
    gauge(
        "antbot_unrealized_pnl_usd",
        "Unrealized PnL of the open position, USD.",
        "",
        s.unrealized_pnl_usd,
    );
    gauge(
        "antbot_position_size",
        "Signed position size (> 0 long).",
        "",
        s.position_size,
    );
    gauge(
        "antbot_open_orders",
        "Open orders known to the executor.",
        "",
        s.open_orders as f64,
    );
    gauge(
        "antbot_trades_closed",
        "Closed trades in the ledger.",
        "",
        s.trades_closed as f64,
    );
    gauge(
        "antbot_premium_bps",
        "Signal premium in bps (NaN = not available / not warm).",
        "",
        opt(s.premium_bps),
    );
    gauge(
        "antbot_raw_premium_bps",
        "Instantaneous premium in bps.",
        "",
        opt(s.raw_premium_bps),
    );
    gauge(
        "antbot_premium_ready",
        "Premium rolling-mean window covered.",
        "",
        b(s.premium_ready),
    );
    out
}

/// Parse the request line of a request head: `(method, target)`.
pub fn parse_request_line(head: &str) -> Option<(&str, &str)> {
    let line = head.lines().next()?;
    let mut it = line.split_ascii_whitespace();
    let (m, t, v) = (it.next()?, it.next()?, it.next()?);
    (v.starts_with("HTTP/1.") && it.next().is_none()).then_some((m, t))
}

/// Read a request head (up to the blank line), bounded by
/// [`MAX_REQUEST_BYTES`].
async fn read_head(sock: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n") {
            break;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return None;
        }
    }
    String::from_utf8(buf).ok()
}

async fn handle(mut sock: TcpStream, rx: watch::Receiver<Arc<HealthSnapshot>>) {
    let resp = match read_head(&mut sock).await {
        Some(head) => match parse_request_line(&head) {
            Some((method, target)) => {
                let snap = rx.borrow().clone();
                let r = route(method, target, &snap, crate::hl::market::now_ms());
                r.to_bytes(method == "HEAD")
            }
            None => HttpResponse::text(400, "bad request").to_bytes(false),
        },
        None => HttpResponse::text(400, "bad request").to_bytes(false),
    };
    let _ = sock.write_all(&resp).await;
    let _ = sock.shutdown().await;
}

/// The running server. Dropping it aborts the accept loop.
pub struct HealthServer {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl HealthServer {
    /// Bind `addr` (must be loopback) and serve the snapshots of `rx`.
    pub async fn bind(
        addr: SocketAddr,
        rx: watch::Receiver<Arc<HealthSnapshot>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            addr.ip().is_loopback(),
            "health server address {addr} is not a loopback address"
        );
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding the health server on {addr}"))?;
        let addr = listener.local_addr()?;
        let (stop, mut stop_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
            loop {
                tokio::select! {
                    _ = stop_rx.changed() => break,
                    r = listener.accept() => match r {
                        Ok((sock, _peer)) => {
                            // Busy: close at once rather than queue.
                            let Ok(permit) = slots.clone().try_acquire_owned() else {
                                continue;
                            };
                            let rx = rx.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                let _ = tokio::time::timeout(CONNECTION_TIMEOUT, handle(sock, rx)).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "health server accept failed");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        });
        tracing::info!(%addr, "health server listening (/healthz, /metrics)");
        Ok(Self { addr, stop, task })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting and wait (bounded) for the accept loop to end.
    pub async fn shutdown(mut self, timeout: Duration) {
        let _ = self.stop.send(true);
        let _ = tokio::time::timeout(timeout, &mut self.task).await;
    }
}

impl Drop for HealthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Status code of an HTTP response (`HTTP/1.1 200 OK` -> 200).
pub fn parse_status_code(resp: &str) -> Option<u16> {
    let line = resp.lines().next()?;
    let mut it = line.split_ascii_whitespace();
    let v = it.next()?;
    if !v.starts_with("HTTP/") {
        return None;
    }
    it.next()?.parse().ok()
}

/// `GET /healthz` on `addr`: the status code and body.
pub async fn probe(addr: SocketAddr, timeout: Duration) -> anyhow::Result<(u16, String)> {
    let work = async {
        let mut s = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to the health server on {addr}"))?;
        let req = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await?;
        let mut buf = Vec::new();
        s.take(256 * 1024).read_to_end(&mut buf).await?;
        let text = String::from_utf8_lossy(&buf).to_string();
        let code = parse_status_code(&text)
            .with_context(|| format!("malformed answer from the health server on {addr}"))?;
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        anyhow::Ok((code, body))
    };
    tokio::time::timeout(timeout, work)
        .await
        .with_context(|| format!("health server on {addr} did not answer within {timeout:?}"))?
}

/// Fallback check without a health server: the run lock in `data_dir`
/// has a fresh heartbeat. `Ok(detail)` / `Err(why)`.
pub fn check_lock(data_dir: &Path, now_ms: u64) -> Result<String, String> {
    match crate::runlock::read(data_dir) {
        None => Err(format!(
            "no run lock in {} (antbot not running there?)",
            data_dir.display()
        )),
        Some(l) if l.is_fresh(now_ms) => Ok(format!(
            "run lock fresh (pid {}, mode {}, heartbeat {} s ago)",
            l.pid,
            l.mode,
            now_ms.saturating_sub(l.heartbeat_ms) / 1000
        )),
        Some(l) => Err(format!(
            "run lock stale: heartbeat {} s ago (pid {}, mode {})",
            now_ms.saturating_sub(l.heartbeat_ms) / 1000,
            l.pid,
            l.mode
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn healthy() -> HealthSnapshot {
        HealthSnapshot {
            started_at_ms: NOW - 60_000,
            updated_at_ms: NOW - 500,
            instance: "antbot".into(),
            mode: "paper".into(),
            coin: "xyz:ANT".into(),
            phase: "flat".into(),
            ws_connected: true,
            ws_stale: false,
            data_age_ms: Some(800),
            stale_threshold_ms: 15_000,
            reconcile_ok: true,
            reconcile_failures: 0,
            position_mismatch: false,
            killed: false,
            kill_rule: None,
            equity_usd: 12.5,
            realized_pnl_usd: 10.0,
            unrealized_pnl_usd: 2.5,
            position_size: -23.91,
            open_orders: 1,
            trades_closed: 3,
            premium_bps: Some(55.25),
            raw_premium_bps: Some(60.0),
            premium_ready: true,
        }
    }

    #[test]
    fn healthz_200_when_healthy() {
        let r = route("GET", "/healthz", &healthy(), NOW);
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json");
        let j: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(j["status"], "ok");
        assert_eq!(j["healthy"], true);
        assert_eq!(j["problems"].as_array().unwrap().len(), 0);
        // Snapshot fields are flattened in.
        for k in [
            "started_at_ms",
            "updated_at_ms",
            "mode",
            "coin",
            "phase",
            "ws_connected",
            "ws_stale",
            "data_age_ms",
            "reconcile_ok",
            "reconcile_failures",
            "position_mismatch",
            "killed",
            "kill_rule",
            "equity_usd",
            "realized_pnl_usd",
            "unrealized_pnl_usd",
            "position_size",
            "open_orders",
            "trades_closed",
            "premium_bps",
            "raw_premium_bps",
            "premium_ready",
        ] {
            assert!(j.get(k).is_some(), "missing {k}");
        }
        assert_eq!(j["premium_bps"], 55.25);
        let back: HealthReport = serde_json::from_str(&r.body).unwrap();
        assert_eq!(back.snapshot, healthy());
    }

    #[test]
    fn healthz_503_on_each_unhealthy_condition() {
        type Breaker = fn(&mut HealthSnapshot);
        let cases: Vec<(&str, Breaker)> = vec![
            ("starting", |s| s.updated_at_ms = 0),
            ("stalled", |s| s.updated_at_ms = NOW - 120_000),
            ("stale", |s| s.ws_stale = true),
            ("no market data", |s| s.data_age_ms = None),
            ("ms old", |s| s.data_age_ms = Some(15_001)),
            ("reconciliation failing (3", |s| {
                s.reconcile_ok = false;
                s.reconcile_failures = 3;
            }),
            ("position mismatch", |s| s.position_mismatch = true),
            ("kill switch active (hit_rate)", |s| {
                s.killed = true;
                s.kill_rule = Some("hit_rate".into());
            }),
        ];
        for (want, f) in cases {
            let mut s = healthy();
            f(&mut s);
            assert!(!s.is_healthy(NOW), "{want}");
            let r = route("GET", "/healthz", &s, NOW);
            assert_eq!(r.status, 503, "{want}");
            let j: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            assert_eq!(j["status"], "unhealthy");
            let problems = j["problems"].to_string();
            assert!(problems.contains(want), "{want}: {problems}");
        }
        // Data exactly at the threshold is still fine.
        let mut s = healthy();
        s.data_age_ms = Some(15_000);
        assert!(s.is_healthy(NOW));
    }

    #[test]
    fn metrics_prometheus_text() {
        let r = route("GET", "/metrics", &healthy(), NOW);
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, PROMETHEUS_CONTENT_TYPE);
        let t = &r.body;
        for line in [
            "antbot_up 1",
            "antbot_healthy 1",
            "antbot_ws_connected 1",
            "antbot_ws_stale 0",
            "antbot_data_age_ms 800",
            "antbot_phase_info{phase=\"flat\"} 1",
            "antbot_killed 0",
            "antbot_position_mismatch 0",
            "antbot_reconcile_ok 1",
            "antbot_reconcile_failures 0",
            "antbot_equity_usd 12.5",
            "antbot_realized_pnl_usd 10",
            "antbot_unrealized_pnl_usd 2.5",
            "antbot_position_size -23.91",
            "antbot_open_orders 1",
            "antbot_trades_closed 3",
            "antbot_premium_bps 55.25",
            "antbot_info{instance=\"antbot\",mode=\"paper\",coin=\"xyz:ANT\"} 1",
        ] {
            assert!(t.lines().any(|l| l == line), "missing {line:?} in\n{t}");
        }
        // Every sample has HELP and TYPE lines; values parse.
        for l in t.lines().filter(|l| !l.starts_with('#')) {
            let name = l.split(['{', ' ']).next().unwrap();
            assert!(t.contains(&format!("# TYPE {name} gauge")), "{name}");
            let v = l.rsplit(' ').next().unwrap();
            assert!(v == "NaN" || v.parse::<f64>().is_ok(), "{l}");
        }
        // Missing values are NaN, labels are escaped.
        let mut s = healthy();
        s.premium_bps = None;
        s.data_age_ms = None;
        s.phase = "we\"ird\\".into();
        let t = prometheus_text(&s, NOW);
        assert!(t.contains("antbot_premium_bps NaN"));
        assert!(t.contains("antbot_data_age_ms NaN"));
        assert!(t.contains("antbot_healthy 0"));
        assert!(
            t.contains(r#"antbot_phase_info{phase="we\"ird\\"} 1"#),
            "{t}"
        );
    }

    #[test]
    fn metrics_json_format() {
        let mut s = healthy();
        s.killed = true;
        let r = route("GET", "/metrics?format=json", &s, NOW);
        // Metrics always answer 200, even when unhealthy.
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json");
        let j: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(j["healthy"], false);
        assert_eq!(j["killed"], true);
        assert_eq!(j["trades_closed"], 3);
        assert_eq!(
            route("GET", "/metrics?x=1&format=json", &s, NOW).content_type,
            "application/json"
        );
    }

    #[test]
    fn routing_errors() {
        let s = healthy();
        assert_eq!(route("GET", "/", &s, NOW).status, 404);
        assert_eq!(route("GET", "/healthz/x", &s, NOW).status, 404);
        assert_eq!(route("POST", "/healthz", &s, NOW).status, 405);
        assert_eq!(route("HEAD", "/healthz", &s, NOW).status, 200);
        let r = route("HEAD", "/healthz", &s, NOW);
        let bytes = String::from_utf8(r.to_bytes(true)).unwrap();
        assert!(bytes.ends_with("\r\n\r\n"), "{bytes}");
        assert!(bytes.contains(&format!("Content-Length: {}", r.body.len())));
        assert_eq!(
            parse_request_line("GET /metrics?format=json HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("GET", "/metrics?format=json"))
        );
        assert_eq!(parse_request_line("garbage"), None);
        assert_eq!(parse_request_line("GET / SPDY/3"), None);
        assert_eq!(
            parse_status_code("HTTP/1.1 503 Service Unavailable\r\n"),
            Some(503)
        );
        assert_eq!(parse_status_code("HTTP/1.0 200 OK"), Some(200));
        assert_eq!(parse_status_code("hello"), None);
    }

    #[test]
    fn lock_fallback() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_lock(dir.path(), NOW)
            .unwrap_err()
            .contains("no run lock"));
        let lock = crate::runlock::RunLock::acquire(dir.path(), "paper", NOW).unwrap();
        assert!(check_lock(dir.path(), NOW + 1_000)
            .unwrap()
            .contains("fresh"));
        let stale = NOW + crate::runlock::LOCK_STALE_MS + 1;
        assert!(check_lock(dir.path(), stale).unwrap_err().contains("stale"));
        drop(lock);
    }

    #[tokio::test]
    async fn server_serves_healthz_and_metrics_then_stops() {
        let (tx, rx) = watch::channel(Arc::new(HealthSnapshot::default()));
        let srv = HealthServer::bind("127.0.0.1:0".parse().unwrap(), rx)
            .await
            .unwrap();
        let addr = srv.local_addr();
        // Nothing published yet: 503.
        let (code, body) = probe(addr, Duration::from_secs(5)).await.unwrap();
        assert_eq!(code, 503, "{body}");
        assert!(body.contains("starting"), "{body}");
        // Healthy snapshot (fresh timestamps): 200.
        let now = crate::hl::market::now_ms();
        let mut s = healthy();
        s.started_at_ms = now - 1_000;
        s.updated_at_ms = now;
        tx.send_replace(Arc::new(s));
        let (code, body) = probe(addr, Duration::from_secs(5)).await.unwrap();
        assert_eq!(code, 200, "{body}");
        // Raw /metrics request.
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(out.contains(PROMETHEUS_CONTENT_TYPE));
        assert!(out.contains("\nantbot_up 1\n"));
        // Garbage request: 400.
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"hello\r\n\r\n").await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
        // Shutdown: the port stops answering.
        srv.shutdown(Duration::from_secs(2)).await;
        assert!(probe(addr, Duration::from_millis(500)).await.is_err());
    }

    #[tokio::test]
    async fn server_refuses_non_loopback() {
        let (_tx, rx) = watch::channel(Arc::new(HealthSnapshot::default()));
        let e = HealthServer::bind("0.0.0.0:0".parse().unwrap(), rx)
            .await
            .err()
            .unwrap();
        assert!(e.to_string().contains("loopback"), "{e}");
    }
}
