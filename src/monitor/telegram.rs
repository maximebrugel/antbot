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
//!   limit), so bursts (kill + exit + warnings) arrive together. An alert
//!   that does not fit is never cut: it is carried over and starts the
//!   next message (only a single alert longer than the limit is
//!   truncated). A `429` answer honours `retry_after`; transport errors
//!   and `5xx` are retried with backoff; other errors drop the message
//!   with a log line, and the next message says how many alerts were not
//!   delivered. A retry after a transport timeout can post the same
//!   message twice (Telegram has no idempotency key): duplicates are
//!   preferred over losses.
//! * **Disabled gracefully:** without a token / chat id (or with
//!   `monitor.telegram_enabled = false`) a single log line says so and
//!   every call is a no-op.
//! * A `200` answer counts as delivered only when its body decodes as a
//!   Bot API response with `ok: true`; an unreadable or malformed body is
//!   retried like a transport error (delivery is unknown, and a duplicate
//!   is preferred over a loss).
//! * The bot token is part of the request URL and could be echoed back by
//!   the API or a proxy: every error string (transport cause chain and API
//!   `description`) is built without the URL and then has every occurrence
//!   of the token replaced by `<redacted>` before it is logged, counted or
//!   truncated, so it never reaches stdout or the log file.
//!
//! `TELEGRAM_API_BASE` overrides the API base URL (default
//! `https://api.telegram.org`), used by the tests with a loopback mock.
//!
//! `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` may instead be read from
//! files named by `TELEGRAM_BOT_TOKEN_FILE` / `TELEGRAM_CHAT_ID_FILE`
//! (Docker secrets; see [`crate::secrets`]). The token is wiped from memory
//! when the config / sender holding it is dropped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use zeroize::{Zeroize, Zeroizing};

use crate::secrets::{env_or_file, SecretError};

use crate::monitor::alerts::{truncate, MAX_ALERT_CHARS};

pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";
pub const ENV_BOT_TOKEN: &str = "TELEGRAM_BOT_TOKEN";
pub const ENV_CHAT_ID: &str = "TELEGRAM_CHAT_ID";
pub const ENV_API_BASE: &str = "TELEGRAM_API_BASE";
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

impl Drop for TelegramConfig {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

impl TelegramConfig {
    /// From the environment (`TELEGRAM_BOT_TOKEN[_FILE]`,
    /// `TELEGRAM_CHAT_ID[_FILE]`, optional `TELEGRAM_API_BASE`). `Ok(None)`
    /// if token or chat id is unset or empty; an error if a variable is set
    /// both directly and as `_FILE`, or its file is missing / unreadable /
    /// empty (the message names the variable and path, never the value).
    pub fn try_from_env(env: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, SecretError> {
        let token = env_or_file(&env, ENV_BOT_TOKEN)?;
        let chat_id = env_or_file(&env, ENV_CHAT_ID)?;
        let (Some(mut token), Some(mut chat_id)) = (token, chat_id) else {
            return Ok(None);
        };
        let api_base = env(ENV_API_BASE)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
            .trim_end_matches('/')
            .to_string();
        // Move the strings out; the emptied Zeroizing shells drop harmlessly.
        Ok(Some(Self {
            token: std::mem::take(&mut *token),
            chat_id: std::mem::take(&mut *chat_id),
            api_base,
        }))
    }

    /// [`Self::try_from_env`], with an error logged (and treated as unset).
    /// `antbot run` / `check-config` call `try_from_env` first and refuse
    /// to start on such an error.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        match Self::try_from_env(env) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "telegram disabled: invalid credentials");
                None
            }
        }
    }

    fn send_url(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("{}/bot{}/sendMessage", self.api_base, self.token))
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
    stats: Arc<Stats>,
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
    /// Messages delivered (one message can hold several alerts).
    pub sent: AtomicU64,
    /// Messages given up after the retries.
    pub failed: AtomicU64,
    /// Alerts inside the failed messages.
    pub failed_alerts: AtomicU64,
    /// Alerts dropped because the queue was full.
    pub dropped: AtomicU64,
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
    pub fn spawn(mut cfg: TelegramConfig, settings: AlertSettings) -> (Self, Arc<Stats>) {
        let stats = Arc::new(Stats::default());
        // No `Client::default()` fallback: it panics when TLS cannot be
        // initialised (the likely reason `build()` failed) and has no
        // timeout. Alerts are disabled instead.
        let http = match reqwest::Client::builder()
            .timeout(settings.http_timeout)
            .user_agent(concat!("antbot/", env!("CARGO_PKG_VERSION")))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    error = %redact(&error_chain(&e.without_url()), &cfg.token),
                    "telegram alerts disabled: cannot build the HTTP client"
                );
                return (Self::disabled(), stats);
            }
        };
        let (tx, rx) = mpsc::channel(settings.queue_capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let worker = Worker {
            http,
            url: cfg.send_url(),
            token: Zeroizing::new(std::mem::take(&mut cfg.token)),
            chat_id: std::mem::take(&mut cfg.chat_id),
            settings,
            rx,
            dropped: dropped.clone(),
            undelivered: 0,
            stats: stats.clone(),
            sent_at: VecDeque::new(),
        };
        tokio::spawn(worker.run());
        (
            Self {
                inner: Some(Arc::new(Inner {
                    tx,
                    dropped,
                    stats: stats.clone(),
                })),
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
            inner.stats.dropped.fetch_add(1, Ordering::Relaxed);
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

    /// Log the delivery counters (at shutdown, after [`Self::flush`]).
    pub fn log_stats(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        let s = &inner.stats;
        tracing::info!(
            sent = s.sent.load(Ordering::Relaxed),
            failed = s.failed.load(Ordering::Relaxed),
            failed_alerts = s.failed_alerts.load(Ordering::Relaxed),
            dropped_queue_full = s.dropped.load(Ordering::Relaxed),
            "telegram alert delivery stats"
        );
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
    /// Holds the token: never logged (wiped on drop).
    url: Zeroizing<String>,
    /// Redacted from every error string before it is logged.
    token: Zeroizing<String>,
    chat_id: String,
    settings: AlertSettings,
    rx: mpsc::Receiver<Msg>,
    dropped: Arc<AtomicU64>,
    /// Alerts of messages given up since the last delivered message.
    undelivered: u64,
    stats: Arc<Stats>,
    /// Send times within the last minute.
    sent_at: VecDeque<Instant>,
}

impl Worker {
    async fn run(mut self) {
        // An alert that did not fit in the previous message.
        let mut carry: Option<String> = None;
        loop {
            let first = match carry.take() {
                Some(t) => t,
                None => match self.rx.recv().await {
                    None => break,
                    Some(Msg::Flush(ack)) => {
                        let _ = ack.send(());
                        continue;
                    }
                    Some(Msg::Text(t)) => t,
                },
            };
            self.wait_turn().await;
            // Merge what queued up meanwhile, within the size budget.
            let dropped = self.dropped.swap(0, Ordering::Relaxed);
            let undelivered = self.undelivered;
            let head = header(&self.settings.prefix, dropped, undelivered);
            let budget = MAX_ALERT_CHARS.saturating_sub(head.chars().count());
            let rx = &mut self.rx;
            let batch = fill_batch(first, budget, || rx.try_recv().ok());
            carry = batch.carry;
            let text = compose_with_header(&head, &batch.texts);
            if self.deliver(&text).await {
                self.undelivered = 0;
            } else {
                // Not delivered: say so in the next message, with the
                // queue-full drops this message was carrying.
                self.undelivered += batch.texts.len() as u64;
                self.dropped.fetch_add(dropped, Ordering::Relaxed);
                self.stats
                    .failed_alerts
                    .fetch_add(batch.texts.len() as u64, Ordering::Relaxed);
            }
            if let Some(ack) = batch.flush {
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

    /// Send one message (with retries). Returns whether it was delivered.
    async fn deliver(&mut self, text: &str) -> bool {
        let mut backoff = self.settings.retry_backoff;
        for attempt in 1..=SEND_ATTEMPTS {
            self.sent_at.push_back(Instant::now());
            match self.post(text).await {
                Ok(()) => {
                    self.stats.sent.fetch_add(1, Ordering::Relaxed);
                    return true;
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
                    return false;
                }
            }
        }
        false
    }

    /// [`Self::post_raw`] with the token redacted from the error (defence
    /// in depth: `post_raw` already redacts what it builds).
    async fn post(&self, text: &str) -> Result<(), SendError> {
        self.post_raw(text).await.map_err(|e| match e {
            SendError::Retry(d, why) => SendError::Retry(d, redact(&why, &self.token)),
            SendError::Fatal(why) => SendError::Fatal(redact(&why, &self.token)),
        })
    }

    async fn post_raw(&self, text: &str) -> Result<(), SendError> {
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        let resp = self
            .http
            .post(self.url.as_str())
            .json(&body)
            .send()
            .await
            // The URL holds the token: strip it, then redact anyway.
            .map_err(|e| {
                SendError::Retry(
                    Duration::ZERO,
                    redact(&error_chain(&e.without_url()), &self.token),
                )
            })?;
        let status = resp.status();
        // A body cut mid-way (or any read error) leaves delivery unknown:
        // retried, like a transport error.
        let raw = resp.text().await.map_err(|e| {
            SendError::Retry(
                Duration::ZERO,
                redact(
                    &format!(
                        "HTTP {status}: reading body: {}",
                        error_chain(&e.without_url())
                    ),
                    &self.token,
                ),
            )
        })?;
        let parsed: Option<TgResponse> = serde_json::from_str(&raw).ok();
        match &parsed {
            Some(r) if status.is_success() && r.ok => return Ok(()),
            None if status.is_success() => {
                return Err(SendError::Retry(
                    Duration::ZERO,
                    format!("HTTP {status}: malformed response body"),
                ));
            }
            _ => {}
        }
        let desc = parsed
            .as_ref()
            .and_then(|r| r.description.as_deref())
            .unwrap_or(if parsed.is_none() { "invalid JSON" } else { "" });
        let why = format!("HTTP {status} {}", sanitize_description(desc, &self.token));
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

/// Remove every occurrence of the bot token (defence in depth on top of
/// `reqwest::Error::without_url`). Shared by the alert worker and the
/// command client.
pub fn redact(s: &str, token: &str) -> String {
    if token.is_empty() {
        return s.to_string();
    }
    s.replace(token, "<redacted>")
}

/// Longest Bot API `description` kept in an error message.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// A Bot API `description` safe to log: the token redacted first (a token
/// straddling the cut would otherwise survive), then cut to
/// [`MAX_DESCRIPTION_CHARS`].
pub fn sanitize_description(desc: &str, token: &str) -> String {
    redact(desc, token)
        .chars()
        .take(MAX_DESCRIPTION_CHARS)
        .collect()
}

/// A reqwest error with its cause chain (timeout, DNS, connection
/// refused...). Strip the URL first (`without_url`): it holds the token;
/// callers redact the result as well.
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        let c_s = c.to_string();
        if !s.contains(&c_s) {
            s.push_str(": ");
            s.push_str(&c_s);
        }
        src = c.source();
    }
    s
}

/// The alerts of one message, and what stopped the merge.
struct Batch {
    texts: Vec<String>,
    /// The next alert, which did not fit: it starts the next message.
    carry: Option<String>,
    /// A flush marker reached: ack it once this message is handled.
    flush: Option<oneshot::Sender<()>>,
}

/// Merge queued alerts (`next` pops the queue) after `first` while the
/// message body stays within `budget` characters. An alert is never cut
/// here: one that does not fit is returned as `carry`. Stops at a flush
/// marker so the flush is acked only after what was queued before it.
fn fill_batch(first: String, budget: usize, mut next: impl FnMut() -> Option<Msg>) -> Batch {
    // Body length as built by `compose_with_header`: "\n" + first, then
    // "\n\n" + each other alert.
    let mut len = 1 + first.chars().count();
    let mut b = Batch {
        texts: vec![first],
        carry: None,
        flush: None,
    };
    while len < budget {
        match next() {
            Some(Msg::Text(t)) => {
                let n = 2 + t.chars().count();
                if len + n > budget {
                    b.carry = Some(t);
                    break;
                }
                len += n;
                b.texts.push(t);
            }
            Some(Msg::Flush(ack)) => {
                b.flush = Some(ack);
                break;
            }
            None => break,
        }
    }
    b
}

/// First line(s) of a message: prefix, then the loss counters.
pub fn header(prefix: &str, dropped: u64, undelivered: u64) -> String {
    let mut s = String::from(prefix);
    if dropped > 0 {
        s.push_str(&format!("\n({dropped} alerts dropped: queue full)"));
    }
    if undelivered > 0 {
        s.push_str(&format!(
            "\n({undelivered} earlier alerts could not be delivered)"
        ));
    }
    s
}

fn compose_with_header(head: &str, texts: &[String]) -> String {
    let mut s = String::from(head);
    for (i, t) in texts.iter().enumerate() {
        s.push_str(if i == 0 { "\n" } else { "\n\n" });
        s.push_str(t);
    }
    // Only cuts a single alert that is too long by itself.
    truncate(&s)
}

/// One Telegram message out of one or more queued alerts.
pub fn compose(prefix: &str, texts: &[String], dropped: u64) -> String {
    compose_with_header(&header(prefix, dropped, 0), texts)
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
    fn config_from_secret_files() {
        let dir = tempfile::tempdir().unwrap();
        let tok = dir.path().join("tok");
        let chat = dir.path().join("chat");
        std::fs::write(&tok, "123456:AAF-secret_token\n").unwrap();
        std::fs::write(&chat, " 4242 \n").unwrap();
        let (tok, chat) = (tok.to_str().unwrap(), chat.to_str().unwrap());
        let c = TelegramConfig::try_from_env(env(&[
            ("TELEGRAM_BOT_TOKEN_FILE", tok),
            ("TELEGRAM_CHAT_ID_FILE", chat),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.token, "123456:AAF-secret_token");
        assert_eq!(c.chat_id, "4242");
        // Redaction covers a token loaded from a file.
        let leaked = format!("error sending to {}: timeout", c.send_url().as_str());
        let r = redact(&leaked, &c.token);
        assert!(!r.contains("AAF-secret_token"), "{r}");
        assert!(!format!("{c:?}").contains("AAF-secret_token"));
        // Mixed: token from a file, chat id from the env.
        let c = TelegramConfig::try_from_env(env(&[
            ("TELEGRAM_BOT_TOKEN_FILE", tok),
            ("TELEGRAM_CHAT_ID", "7"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.chat_id, "7");
        // Both forms: explicit error, never the value.
        let e = TelegramConfig::try_from_env(env(&[
            ("TELEGRAM_BOT_TOKEN", "123456:AAF-other"),
            ("TELEGRAM_BOT_TOKEN_FILE", tok),
            ("TELEGRAM_CHAT_ID", "7"),
        ]))
        .unwrap_err();
        assert!(e.to_string().contains("TELEGRAM_BOT_TOKEN_FILE"), "{e}");
        assert!(!e.to_string().contains("AAF"), "{e}");
        // Missing file: explicit error; from_env logs it and disables alerts.
        let missing = dir.path().join("missing");
        let bad = [
            ("TELEGRAM_BOT_TOKEN_FILE", missing.to_str().unwrap()),
            ("TELEGRAM_CHAT_ID", "7"),
        ];
        assert!(matches!(
            TelegramConfig::try_from_env(env(&bad)),
            Err(SecretError::Unreadable { .. })
        ));
        assert_eq!(TelegramConfig::from_env(env(&bad)), None);
        // Neither: Ok(None).
        assert_eq!(TelegramConfig::try_from_env(env(&[])), Ok(None));
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
            c.send_url().as_str(),
            "https://api.telegram.org/bot123:abc/sendMessage"
        );
        let c = TelegramConfig::from_env(env(&[
            ("TELEGRAM_BOT_TOKEN", "t"),
            ("TELEGRAM_CHAT_ID", "1"),
            ("TELEGRAM_API_BASE", "http://127.0.0.1:9/"),
        ]))
        .unwrap();
        assert_eq!(c.send_url().as_str(), "http://127.0.0.1:9/bott/sendMessage");
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
            "[antbot]\n(2 alerts dropped: queue full)\na\n\nb"
        );
        assert_eq!(
            compose_with_header(&header("[antbot]", 0, 3), &["a".into()]),
            "[antbot]\n(3 earlier alerts could not be delivered)\na"
        );
    }

    /// Drain `queue` the way the worker does; returns the composed
    /// messages.
    fn drain_like_worker(prefix: &str, queue: Vec<String>) -> Vec<String> {
        let mut q: std::collections::VecDeque<Msg> = queue.into_iter().map(Msg::Text).collect();
        let mut out = Vec::new();
        let mut carry: Option<String> = None;
        loop {
            let first = match carry.take() {
                Some(t) => t,
                None => match q.pop_front() {
                    Some(Msg::Text(t)) => t,
                    Some(Msg::Flush(_)) => continue,
                    None => break,
                },
            };
            let head = header(prefix, 7, 0);
            let budget = MAX_ALERT_CHARS - head.chars().count();
            let b = fill_batch(first, budget, || q.pop_front());
            carry = b.carry;
            out.push(compose_with_header(&head, &b.texts));
        }
        out
    }

    #[test]
    fn merged_burst_never_cuts_the_kill_alert() {
        // ~30 warnings of ~125 chars, then the kill alert, then more.
        let mut queue: Vec<String> = (0..30)
            .map(|i| {
                format!(
                    "⚠️ userFills resync failed (attempt {i:02}): {}",
                    "x".repeat(80)
                )
            })
            .collect();
        let kill = crate::monitor::alerts::kill(
            &format!("daily loss -151.20 USD <= -150 USD {}", "y".repeat(200)),
            true,
        );
        queue.push(kill.clone());
        queue.extend((0..40).map(|i| format!("⚠️ rate limited {i}: {}", "z".repeat(100))));
        let msgs = drain_like_worker("[antbot paper xyz:ANT]", queue.clone());
        assert!(msgs.len() >= 2);
        for m in &msgs {
            assert!(
                m.chars().count() <= MAX_ALERT_CHARS,
                "{}",
                m.chars().count()
            );
            assert!(!m.contains("(truncated)"));
        }
        // The kill alert arrives whole, exactly once.
        assert_eq!(msgs.iter().filter(|m| m.contains(&kill)).count(), 1);
        // Every alert is delivered, in order, exactly once.
        let all = msgs.join("\n\n");
        let mut from = 0;
        for t in &queue {
            let at = all[from..].find(t.as_str()).expect("alert lost") + from;
            from = at + t.len();
        }
    }

    #[test]
    fn single_oversized_alert_is_truncated_alone() {
        let big = "b".repeat(MAX_ALERT_CHARS + 100);
        let msgs = drain_like_worker("[antbot]", vec!["small".into(), big, "after".into()]);
        assert_eq!(msgs.len(), 3);
        assert!(msgs[0].ends_with("small"));
        assert!(msgs[1].ends_with("(truncated)"));
        assert!(msgs[1].chars().count() <= MAX_ALERT_CHARS);
        assert!(msgs[2].ends_with("after"));
    }

    #[test]
    fn fill_batch_stops_at_flush_marker() {
        let (ack, _rx) = oneshot::channel();
        let mut q: std::collections::VecDeque<Msg> = vec![
            Msg::Text("b".into()),
            Msg::Flush(ack),
            Msg::Text("c".into()),
        ]
        .into();
        let b = fill_batch("a".into(), 1000, || q.pop_front());
        assert_eq!(b.texts, vec!["a".to_string(), "b".to_string()]);
        assert!(b.flush.is_some());
        assert!(b.carry.is_none());
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn error_chain_includes_causes() {
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let e = Outer(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        assert_eq!(error_chain(&e), "error sending request: connection refused");
    }
}
