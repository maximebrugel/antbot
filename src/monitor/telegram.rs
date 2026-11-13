//! Telegram alerts (`TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID`).
//!
//! * **Non-blocking:** [`Alerter::send`] only pushes onto a bounded queue
//!   (`try_send`); a background task does the HTTP calls. When the queue is
//!   full the alert is dropped and counted, and the next message says how
//!   many were lost. The trading loop never waits on Telegram.
//! * **Rate-limited:** at least `min_interval` between two messages and at
//!   most `max_per_min` in any rolling minute (Telegram allows about one
//!   message per second per chat, 20 per minute in groups). Alerts already
//!   queued while waiting are merged into one message (up to the size
//!   limit), so bursts (kill + exit + warnings) arrive together. A `429`
//!   answer honours `retry_after`; transport errors and `5xx` are retried
//!   with backoff; other errors drop the message with a log line.
//! * **Disabled gracefully:** without a token / chat id (or with
//!   `monitor.telegram_enabled = false`) a single log line says so and
//!   every call is a no-op.
//! * The bot token is part of the request URL: errors are logged without
//!   the URL so it never reaches the logs.
//!
//! `TELEGRAM_API_BASE` overrides the API base URL (default
//! `https://api.telegram.org`), used by the tests with a loopback mock.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::monitor::alerts::{truncate, MAX_ALERT_CHARS};

pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";
const SEND_ATTEMPTS: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Bot credentials and endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct TelegramConfig {
    pub token: String,
    pub chat_id: String,
    pub api_base: String,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl TelegramConfig {
    /// From the environment (`TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`,
    /// optional `TELEGRAM_API_BASE`). `None` if token or chat id is unset
    /// or empty.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let get = |k: &str| {
            env(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Some(Self {
            token: get("TELEGRAM_BOT_TOKEN")?,
            chat_id: get("TELEGRAM_CHAT_ID")?,
            api_base: get("TELEGRAM_API_BASE")
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
                .trim_end_matches('/')
                .to_string(),
        })
    }

    fn send_url(&self) -> String {
        format!("{}/bot{}/sendMessage", self.api_base, self.token)
    }
}

/// Queue and pacing settings.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertSettings {
    /// Header line of every message (e.g. `[antbot paper xyz:ANT]`).
    pub prefix: String,
    pub queue_capacity: usize,
    pub max_per_min: u32,
    pub min_interval: Duration,
    pub http_timeout: Duration,
    /// First retry delay for transport errors / 5xx (doubles).
    pub retry_backoff: Duration,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            prefix: "[antbot]".into(),
            queue_capacity: 256,
            max_per_min: 20,
            min_interval: Duration::from_millis(1100),
            http_timeout: Duration::from_secs(10),
            retry_backoff: Duration::from_secs(1),
        }
    }
}

enum Msg {
    Text(String),
    Flush(oneshot::Sender<()>),
}

struct Inner {
    tx: mpsc::Sender<Msg>,
    dropped: Arc<AtomicU64>,
}

/// Cheap-to-clone handle on the alert queue (no-op when disabled).
#[derive(Clone, Default)]
pub struct Alerter {
    inner: Option<Arc<Inner>>,
}

impl std::fmt::Debug for Alerter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Alerter")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

/// Delivery counters (for tests and the shutdown log).
#[derive(Debug, Default)]
pub struct Stats {
    pub sent: AtomicU64,
    pub failed: AtomicU64,
}

impl Alerter {
    /// A disabled alerter (every call is a no-op), without a log line.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Build from config + environment. Logs once whether alerts are on.
    /// Must run inside a tokio runtime when enabled.
    pub fn from_env(
        enabled_in_config: bool,
        env: impl Fn(&str) -> Option<String>,
        settings: AlertSettings,
    ) -> Self {
        if !enabled_in_config {
            tracing::info!("telegram alerts disabled (monitor.telegram_enabled = false)");
            return Self::disabled();
        }
        match TelegramConfig::from_env(env) {
            Some(cfg) => {
                tracing::info!(
                    chat_id = %cfg.chat_id,
                    custom_api_base = cfg.api_base != DEFAULT_API_BASE,
                    max_per_min = settings.max_per_min,
                    "telegram alerts enabled"
                );
                Self::spawn(cfg, settings).0
            }
            None => {
                tracing::info!(
                    "telegram alerts disabled: TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID not set"
                );
                Self::disabled()
            }
        }
    }

    /// Start the sender task.
    pub fn spawn(cfg: TelegramConfig, settings: AlertSettings) -> (Self, Arc<Stats>) {
        let (tx, rx) = mpsc::channel(settings.queue_capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let stats = Arc::new(Stats::default());
        let worker = Worker {
            http: reqwest::Client::builder()
                .timeout(settings.http_timeout)
                .user_agent(concat!("antbot/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            url: cfg.send_url(),
            chat_id: cfg.chat_id,
            settings,
            rx,
            dropped: dropped.clone(),
            stats: stats.clone(),
            sent_at: VecDeque::new(),
        };
        tokio::spawn(worker.run());
        (
            Self {
                inner: Some(Arc::new(Inner { tx, dropped })),
            },
            stats,
        )
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Queue an alert; never blocks. Dropped (and counted) if the queue is
    /// full.
    pub fn send(&self, text: impl Into<String>) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.tx.try_send(Msg::Text(text.into())).is_err() {
            let n = inner.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 50 == 0 {
                tracing::warn!(dropped = n, "telegram alert queue full: alert dropped");
            }
        }
    }

    /// Wait until every alert queued so far was handled (sent or given up),
    /// at most `timeout`. Used at shutdown so the last alerts go out.
    pub async fn flush(&self, timeout: Duration) -> bool {
        let Some(inner) = &self.inner else {
            return true;
        };
        let (tx, rx) = oneshot::channel();
        let fut = async {
            inner.tx.send(Msg::Flush(tx)).await.ok()?;
            rx.await.ok()
        };
        let done = tokio::time::timeout(timeout, fut)
            .await
            .ok()
            .flatten()
            .is_some();
        if !done {
            tracing::warn!(
                ?timeout,
                "telegram alerts not all delivered before shutdown"
            );
        }
        done
    }
}

#[derive(Debug, Deserialize)]
struct TgResponse {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<TgParams>,
}

#[derive(Debug, Deserialize)]
struct TgParams {
    #[serde(default)]
    retry_after: Option<u64>,
}

enum SendError {
    /// Worth retrying after the given delay.
    Retry(Duration, String),
    Fatal(String),
}

struct Worker {
    http: reqwest::Client,
    url: String,
    chat_id: String,
    settings: AlertSettings,
    rx: mpsc::Receiver<Msg>,
    dropped: Arc<AtomicU64>,
    stats: Arc<Stats>,
    /// Send times within the last minute.
    sent_at: VecDeque<Instant>,
}

impl Worker {
    async fn run(mut self) {
        let mut pending_flush: Option<oneshot::Sender<()>> = None;
        while let Some(msg) = self.rx.recv().await {
            let first = match msg {
                Msg::Flush(ack) => {
                    let _ = ack.send(());
                    continue;
                }
                Msg::Text(t) => t,
            };
            self.wait_turn().await;
            // Merge what queued up meanwhile (stop at a flush marker).
            let mut texts = vec![first];
            let mut len = texts[0].chars().count();
            while len < MAX_ALERT_CHARS {
                match self.rx.try_recv() {
                    Ok(Msg::Text(t)) => {
                        len += t.chars().count() + 2;
                        texts.push(t);
                    }
                    Ok(Msg::Flush(ack)) => {
                        pending_flush = Some(ack);
                        break;
                    }
                    Err(_) => break,
                }
            }
            let dropped = self.dropped.swap(0, Ordering::Relaxed);
            let text = compose(&self.settings.prefix, &texts, dropped);
            self.deliver(&text).await;
            if let Some(ack) = pending_flush.take() {
                let _ = ack.send(());
            }
        }
    }

    /// Respect the minimum gap and the per-minute cap.
    async fn wait_turn(&mut self) {
        let now = Instant::now();
        let at = next_slot(
            &mut self.sent_at,
            now,
            self.settings.min_interval,
            self.settings.max_per_min,
        );
        if at > now {
            tokio::time::sleep_until(at).await;
        }
    }

    async fn deliver(&mut self, text: &str) {
        let mut backoff = self.settings.retry_backoff;
        for attempt in 1..=SEND_ATTEMPTS {
            self.sent_at.push_back(Instant::now());
            match self.post(text).await {
                Ok(()) => {
                    self.stats.sent.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(SendError::Retry(wait, why)) if attempt < SEND_ATTEMPTS => {
                    let wait = if wait.is_zero() { backoff } else { wait };
                    tracing::warn!(attempt, retry_in = ?wait, error = %why, "telegram send failed, retrying");
                    tokio::time::sleep(wait).await;
                    backoff = backoff.saturating_mul(2);
                }
                Err(SendError::Retry(_, why)) | Err(SendError::Fatal(why)) => {
                    // warn, not error: errors are alerted, this must not loop.
                    tracing::warn!(attempt, error = %why, "telegram alert not delivered");
                    self.stats.failed.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
    }

    async fn post(&self, text: &str) -> Result<(), SendError> {
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            // The URL holds the token: never log it.
            .map_err(|e| SendError::Retry(Duration::ZERO, e.without_url().to_string()))?;
        let status = resp.status();
        let parsed: Option<TgResponse> = resp.json().await.ok();
        if status.is_success() && parsed.as_ref().is_none_or(|r| r.ok) {
            return Ok(());
        }
        let desc = parsed
            .as_ref()
            .and_then(|r| r.description.clone())
            .unwrap_or_default();
        let why = format!("HTTP {status} {desc}");
        if status.as_u16() == 429 {
            let after = parsed
                .and_then(|r| r.parameters)
                .and_then(|p| p.retry_after)
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(1))
                .min(MAX_RETRY_AFTER);
            Err(SendError::Retry(after, why))
        } else if status.is_server_error() {
            Err(SendError::Retry(Duration::ZERO, why))
        } else {
            Err(SendError::Fatal(why))
        }
    }
}

/// Earliest time the next message may go out, given the send times of
/// the last minute (older entries are pruned).
pub fn next_slot(
    sent_at: &mut VecDeque<Instant>,
    now: Instant,
    min_interval: Duration,
    max_per_min: u32,
) -> Instant {
    const MINUTE: Duration = Duration::from_secs(60);
    while sent_at
        .front()
        .is_some_and(|t| now.duration_since(*t) >= MINUTE)
    {
        sent_at.pop_front();
    }
    let mut at = now;
    if let Some(last) = sent_at.back() {
        at = at.max(*last + min_interval);
    }
    let cap = max_per_min.max(1) as usize;
    if sent_at.len() >= cap {
        // The oldest of the last `cap` sends must leave the window.
        at = at.max(sent_at[sent_at.len() - cap] + MINUTE);
    }
    at
}

/// One Telegram message out of one or more queued alerts.
pub fn compose(prefix: &str, texts: &[String], dropped: u64) -> String {
    let mut s = String::from(prefix);
    if dropped > 0 {
        s.push_str(&format!("\n({dropped} alerts dropped: queue full)"));
    }
    for t in texts {
        s.push_str(if s == prefix { "\n" } else { "\n\n" });
        s.push_str(t);
    }
    truncate(&s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn config_from_env() {
        assert_eq!(TelegramConfig::from_env(env(&[])), None);
        assert_eq!(
            TelegramConfig::from_env(env(&[("TELEGRAM_BOT_TOKEN", "123:abc")])),
            None
        );
        assert_eq!(
            TelegramConfig::from_env(env(&[
                ("TELEGRAM_BOT_TOKEN", "123:abc"),
                ("TELEGRAM_CHAT_ID", "  ")
            ])),
            None
        );
        let c = TelegramConfig::from_env(env(&[
            ("TELEGRAM_BOT_TOKEN", "123:abc"),
            ("TELEGRAM_CHAT_ID", "-10042"),
        ]))
        .unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert_eq!(
            c.send_url(),
            "https://api.telegram.org/bot123:abc/sendMessage"
        );
        let c = TelegramConfig::from_env(env(&[
            ("TELEGRAM_BOT_TOKEN", "t"),
            ("TELEGRAM_CHAT_ID", "1"),
            ("TELEGRAM_API_BASE", "http://127.0.0.1:9/"),
        ]))
        .unwrap();
        assert_eq!(c.send_url(), "http://127.0.0.1:9/bott/sendMessage");
        assert!(!format!("{c:?}").contains("\"t\""));
        assert!(format!("{c:?}").contains("<redacted>"));
    }

    #[test]
    fn disabled_without_env_or_by_config() {
        // No runtime needed: nothing is spawned when disabled.
        let a = Alerter::from_env(true, env(&[]), AlertSettings::default());
        assert!(!a.is_enabled());
        a.send("ignored");
        let a = Alerter::from_env(
            false,
            env(&[("TELEGRAM_BOT_TOKEN", "t"), ("TELEGRAM_CHAT_ID", "1")]),
            AlertSettings::default(),
        );
        assert!(!a.is_enabled());
    }

    #[test]
    fn pacing_slots() {
        let t0 = Instant::now();
        let gap = Duration::from_millis(1100);
        let mut q = VecDeque::new();
        // Nothing sent: now.
        assert_eq!(next_slot(&mut q, t0, gap, 3), t0);
        // Min interval after the last send.
        q.push_back(t0);
        assert_eq!(next_slot(&mut q, t0, gap, 3), t0 + gap);
        // Per-minute cap: 3 sends at t0, t0+2s, t0+4s -> next at t0+60s.
        q.push_back(t0 + Duration::from_secs(2));
        q.push_back(t0 + Duration::from_secs(4));
        let now = t0 + Duration::from_secs(5);
        assert_eq!(next_slot(&mut q, now, gap, 3), t0 + Duration::from_secs(60));
        // A minute later the oldest send is pruned.
        let later = t0 + Duration::from_secs(61);
        assert_eq!(next_slot(&mut q, later, gap, 3), later);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn compose_messages() {
        assert_eq!(compose("[antbot]", &["a".into()], 0), "[antbot]\na");
        assert_eq!(
            compose("[antbot]", &["a".into(), "b".into()], 2),
            "[antbot]\n(2 alerts dropped: queue full)\n\na\n\nb"
        );
    }
}
