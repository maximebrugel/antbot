//! Inbound Telegram commands: `/pnl`, `/trades`, `/help` (read-only).
//!
//! * **Read-only by construction:** the task only holds a
//!   [`watch::Receiver`] of a [`StatusSnapshot`] (published by the main
//!   loop) and an HTTP client. It has no access to the engine, the
//!   executor or the kill switch, so no command can change state, place
//!   or cancel orders, or stop the bot.
//! * **Never blocks trading:** the main loop publishes with
//!   `send_replace` (non-blocking). The task takes `borrow().clone()` of
//!   an `Arc` and releases the lock at once; no lock is held across
//!   network I/O.
//! * **Contained failures:** transport errors, HTTP 4xx / 5xx (including
//!   409 conflict: another `getUpdates` consumer or a webhook), malformed
//!   JSON are logged (token redacted) and retried with exponential backoff
//!   (1 s doubling up to 60 s). A panic in the poll loop is caught by a
//!   supervisor that logs it and restarts the loop. The offset is advanced
//!   before an update is handled, so an update that made the loop panic
//!   is never processed again.
//! * **Hard authorization:** an update is handled only if it is a new
//!   `message` with `chat.id == from.id == TELEGRAM_CHAT_ID` and
//!   `chat.type == "private"`, sent by a human user. Everything else
//!   (other chats or users, groups, channels, edited messages, callback
//!   queries, non-text messages, the backlog from before startup) is
//!   ignored with at most a debug log line, and never answered. The offset
//!   still advances past it.
//! * Replies are plain text (no `parse_mode`), so nothing needs escaping,
//!   and are kept under the Telegram size limit.

use std::future::Future;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::engine::Engine;
use crate::monitor::alerts::MAX_ALERT_CHARS;
use crate::monitor::telegram::TelegramConfig;
use crate::risk::kill::utc_day;
use crate::strategy::premium::Side;
use crate::strategy::signal::TradeRecord;

/// Trades kept in the snapshot and listed by `/trades`.
pub const TRADES_SHOWN: usize = 20;
/// `getUpdates` long-poll timeout (seconds, server side).
pub const LONG_POLL_SECS: u64 = 25;

// ---------------------------------------------------------------------------
// Snapshot published by the main loop
// ---------------------------------------------------------------------------

/// The open position, as shown by `/pnl`.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionInfo {
    pub side: Side,
    pub size: f64,
    pub entry_px: Option<f64>,
    pub entry_premium_bps: Option<f64>,
    pub opened_ms: u64,
}

/// Active kill switch.
#[derive(Debug, Clone, PartialEq)]
pub struct KillInfo {
    pub rule: String,
    pub at_ms: u64,
    pub message: String,
}

/// Read-only copy of the live state the heartbeat reports, published by
/// the main loop for the command task.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatusSnapshot {
    /// Header of every reply (e.g. `[antbot paper xyz:ANT]`).
    pub prefix: String,
    /// When the snapshot was taken (0 = the main loop has not published
    /// yet).
    pub as_of_ms: u64,
    pub phase: String,
    pub position: Option<PositionInfo>,
    /// Signal premium (rolling mean when configured).
    pub premium_bps: Option<f64>,
    /// Instantaneous premium of the last fresh view.
    pub raw_premium_bps: Option<f64>,
    /// The premium rolling-mean window is covered (signal available).
    pub premium_ready: bool,
    pub premium_stale: bool,
    pub mark_px: Option<f64>,
    pub realized_total_usd: f64,
    pub realized_today_usd: f64,
    pub trades_today: usize,
    /// Open position mark-to-market, net of its fees and funding so far.
    pub unrealized_usd: f64,
    /// Strategy equity: realized + unrealized.
    pub equity_usd: f64,
    /// Fees of closed trades plus the open position's fees so far.
    pub fees_paid_usd: f64,
    pub trades_total: usize,
    /// Executor account position (venue in live modes, simulated account
    /// in paper); shown only when it differs from the strategy's.
    pub venue_position: Option<f64>,
    pub kill: Option<KillInfo>,
    /// Last [`TRADES_SHOWN`] closed trades, in ledger (chronological) order.
    pub recent_trades: Vec<TradeRecord>,
}

impl StatusSnapshot {
    /// Copy what the commands need out of the engine (main loop only;
    /// cheap: at most [`TRADES_SHOWN`] trades are cloned).
    pub fn from_engine(
        engine: &Engine,
        prefix: &str,
        now_ms: u64,
        venue_position: Option<f64>,
    ) -> Self {
        let today = utc_day(now_ms);
        let trades = &engine.ledger.trades;
        let (realized_today_usd, trades_today) = trades
            .iter()
            .filter(|t| utc_day(t.closed_ms) == today)
            .fold((0.0, 0), |(s, n), t| (s + t.pnl_usd, n + 1));
        let pos = engine.strategy.position();
        let unrealized_usd = match (pos, engine.last_mark()) {
            (Some(p), Some(m)) => p.unrealized_pnl(m),
            _ => 0.0,
        };
        let fees_paid_usd =
            trades.iter().map(|t| t.fees_usd).sum::<f64>() + pos.map_or(0.0, |p| p.fees_usd) + 0.0;
        let view = engine.last_view();
        Self {
            prefix: prefix.to_string(),
            as_of_ms: now_ms,
            phase: engine.strategy.phase().kind().as_str().to_string(),
            position: pos.map(|p| PositionInfo {
                side: p.side,
                size: p.size,
                entry_px: p.avg_entry_px(),
                entry_premium_bps: p.entry_premium_bps,
                opened_ms: p.opened_ms,
            }),
            premium_bps: view.and_then(|v| v.premium_bps),
            raw_premium_bps: engine.last_raw_premium_bps(),
            premium_ready: engine.premium_ready(),
            premium_stale: view.is_none_or(|v| v.stale),
            mark_px: engine.last_mark(),
            realized_total_usd: engine.ledger.realized_pnl_usd,
            realized_today_usd: realized_today_usd + 0.0,
            trades_today,
            unrealized_usd,
            equity_usd: engine.equity(),
            fees_paid_usd,
            trades_total: trades.len(),
            venue_position,
            kill: engine.risk.kill_record().map(|k| KillInfo {
                rule: k.reason.rule().to_string(),
                at_ms: k.at_ms,
                message: k.message.clone(),
            }),
            recent_trades: trades[trades.len().saturating_sub(TRADES_SHOWN)..].to_vec(),
        }
    }
}

// ---------------------------------------------------------------------------
// Authorization and parsing (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TgUser {
    id: i64,
    #[serde(default)]
    is_bot: bool,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    /// Unix seconds.
    date: i64,
    chat: TgChat,
    #[serde(default)]
    from: Option<TgUser>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgUpdate {
    #[serde(default)]
    message: Option<TgMessage>,
}

/// What to do with one update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Authorized text message: answer it.
    Accept(String),
    /// Not answered (the reason is for debug logs only).
    Ignore(&'static str),
}

/// `update_id` of a raw update (even a malformed one).
pub fn update_id(u: &Value) -> Option<i64> {
    u.get("update_id").and_then(Value::as_i64)
}

/// The authorization filter. `chat_id` is the configured
/// `TELEGRAM_CHAT_ID`; messages dated before `min_date` (Unix seconds:
/// the backlog from before startup) are ignored.
pub fn authorize(u: &Value, chat_id: i64, min_date: i64) -> Decision {
    if u.get("message").is_none() {
        const KINDS: [(&str, &str); 5] = [
            ("edited_message", "edited message"),
            ("channel_post", "channel post"),
            ("edited_channel_post", "edited channel post"),
            ("callback_query", "callback query"),
            ("inline_query", "inline query"),
        ];
        let why = KINDS
            .iter()
            .find(|(k, _)| u.get(k).is_some())
            .map_or("not a message", |(_, why)| why);
        return Decision::Ignore(why);
    }
    let Ok(TgUpdate { message: Some(m) }) = TgUpdate::deserialize(u) else {
        return Decision::Ignore("malformed message");
    };
    if m.chat.kind != "private" {
        return Decision::Ignore("not a private chat");
    }
    if m.chat.id != chat_id {
        return Decision::Ignore("wrong chat id");
    }
    let Some(from) = m.from else {
        return Decision::Ignore("no sender");
    };
    if from.id != chat_id {
        return Decision::Ignore("wrong sender id");
    }
    if from.is_bot {
        return Decision::Ignore("sender is a bot");
    }
    if m.date < min_date {
        return Decision::Ignore("sent before startup");
    }
    match m.text {
        Some(t) => Decision::Accept(t),
        None => Decision::Ignore("not a text message"),
    }
}

/// A command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Pnl,
    Trades,
    Help,
}

impl Cmd {
    pub fn as_str(self) -> &'static str {
        match self {
            Cmd::Pnl => "pnl",
            Cmd::Trades => "trades",
            Cmd::Help => "help",
        }
    }
}

/// `/pnl`, `/PNL`, `/pnl@antbot_bot`, `/pnl extra args` -> Pnl. Unknown
/// commands and plain text -> Help.
pub fn parse_command(text: &str) -> Cmd {
    let Some(rest) = text.trim_start().strip_prefix('/') else {
        return Cmd::Help;
    };
    let word = rest.split(char::is_whitespace).next().unwrap_or("");
    let name = word.split('@').next().unwrap_or("");
    match name.to_ascii_lowercase().as_str() {
        "pnl" => Cmd::Pnl,
        "trades" => Cmd::Trades,
        _ => Cmd::Help,
    }
}

// ---------------------------------------------------------------------------
// Replies (pure)
// ---------------------------------------------------------------------------

/// `YYYY-MM-DD HH:MM:SS UTC`.
fn utc_secs(ms: u64) -> String {
    let s = crate::monitor::fmt_utc_ms(ms);
    format!("{} {} UTC", &s[..10], &s[11..19])
}

/// `YYYY-MM-DD HH:MM`.
fn utc_minutes(ms: u64) -> String {
    let s = crate::monitor::fmt_utc_ms(ms);
    format!("{} {}", &s[..10], &s[11..16])
}

fn side_up(s: Side) -> &'static str {
    match s {
        Side::Long => "LONG",
        Side::Short => "SHORT",
    }
}

/// Round to `decimals` for display; `+ 0.0` turns -0.0 into 0.0, so a
/// tiny negative value never prints as "-0.00".
fn round_dp(v: f64, decimals: i32) -> f64 {
    let k = 10f64.powi(decimals);
    (v * k).round() / k + 0.0
}

fn bps(v: Option<f64>) -> String {
    v.filter(|x| x.is_finite())
        .map_or_else(|| "n/a".into(), |x| format!("{:+.1}", round_dp(x, 1)))
}

fn usd(v: f64) -> String {
    if !v.is_finite() {
        return "n/a".into();
    }
    format!("{:+.2}", round_dp(v, 2))
}

/// A size or position without float noise (`24.500000000000004` -> `24.5`).
fn qty(v: f64) -> String {
    if !v.is_finite() {
        return "n/a".into();
    }
    format!("{}", round_dp(v, 8))
}

fn held(ms: u64) -> String {
    let m = ms / 60_000;
    format!("{}h{:02}m", m / 60, m % 60)
}

/// Cut to `max` characters on a char boundary, with a note.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let note = "\n…(truncated)";
    let mut out: String = s
        .chars()
        .take(max.saturating_sub(note.chars().count()))
        .collect();
    out.push_str(note);
    out
}

/// The commands list.
pub fn help_text(prefix: &str) -> String {
    format!(
        "{prefix}\nRead-only commands:\n\
         /pnl - PnL (realized, today, unrealized), equity, fees, position, kill switch\n\
         /trades - last {TRADES_SHOWN} closed trades, newest first\n\
         /help - this list\n\
         Commands never change the bot's state or orders."
    )
}

fn not_ready(s: &StatusSnapshot) -> Option<String> {
    (s.as_of_ms == 0).then(|| {
        format!(
            "{}\nstate not available yet (the bot is starting), try again in a few seconds",
            if s.prefix.is_empty() {
                "[antbot]"
            } else {
                &s.prefix
            }
        )
    })
}

/// `/pnl` reply.
pub fn format_pnl(s: &StatusSnapshot, now_ms: u64) -> String {
    if let Some(t) = not_ready(s) {
        return t;
    }
    let mut o = format!("{}\nstatus as of {}", s.prefix, utc_secs(s.as_of_ms));
    o.push_str(&format!("\nphase: {}", s.phase));
    let signal = match s.premium_bps.filter(|x| x.is_finite()) {
        Some(x) => format!("{x:+.1} bps"),
        None if !s.premium_ready => "n/a (averaging window warming up)".to_string(),
        None => "n/a".to_string(),
    };
    match &s.position {
        Some(p) => {
            let px = p
                .entry_px
                .filter(|x| x.is_finite())
                .map_or_else(|| "n/a".into(), |x| format!("{x:.4}"));
            o.push_str(&format!(
                "\nposition: {} {} @ {px}, held {}",
                side_up(p.side),
                qty(p.size),
                held(now_ms.saturating_sub(p.opened_ms))
            ));
            o.push_str(&format!(
                "\npremium: entry {} bps, signal {}",
                bps(p.entry_premium_bps),
                signal
            ));
        }
        None => {
            o.push_str("\nposition: flat");
            o.push_str(&format!("\npremium: signal {signal}"));
        }
    }
    o.push_str(&format!(" (raw {} bps", bps(s.raw_premium_bps)));
    o.push_str(if s.premium_stale {
        ", data stale)"
    } else {
        ")"
    });
    // The executor's account (venue in live, simulated in paper): only
    // shown when it disagrees with the strategy.
    let ours = s.position.as_ref().map_or(0.0, |p| p.side.sign() * p.size);
    if let Some(v) = s.venue_position.filter(|v| (v - ours).abs() > 1e-9) {
        o.push_str(&format!(
            "\n⚠️ account position {} differs from the strategy position {}",
            qty(v),
            qty(ours)
        ));
    }
    o.push_str(&format!(
        "\nrealized PnL: {} USD total, {} USD today (UTC, {} trade{})",
        usd(s.realized_total_usd),
        usd(s.realized_today_usd),
        s.trades_today,
        if s.trades_today == 1 { "" } else { "s" }
    ));
    o.push_str(&format!(
        "\nunrealized PnL: {} USD{}",
        usd(s.unrealized_usd),
        if s.position.is_some() {
            " (net of fees / funding so far)"
        } else {
            ""
        }
    ));
    o.push_str(&format!(
        "\nequity (realized + unrealized): {} USD",
        usd(s.equity_usd)
    ));
    o.push_str(&format!(
        "\nfees paid: {:.4} USD",
        round_dp(s.fees_paid_usd, 4)
    ));
    o.push_str(&format!("\ntrades: {}", s.trades_total));
    match &s.kill {
        Some(k) => o.push_str(&format!(
            "\n⛔ kill switch ACTIVE ({}) since {}: {}",
            k.rule,
            utc_secs(k.at_ms),
            k.message
        )),
        None => o.push_str("\nkill switch: not active"),
    }
    clip(&o, MAX_ALERT_CHARS)
}

/// One `/trades` line.
fn trade_line(t: &TradeRecord) -> String {
    format!(
        "{} {} {} | prem {} -> {} bps | {} USD | {}",
        utc_minutes(t.closed_ms),
        side_up(t.side),
        qty(t.size),
        bps(t.entry_premium_bps),
        bps(t.exit_premium_bps),
        usd(t.pnl_usd),
        t.exit_reason.as_str()
    )
}

/// `/trades` reply: up to [`TRADES_SHOWN`] trades, newest first, within
/// `max_chars` (lines that do not fit are dropped with a note).
pub fn format_trades(s: &StatusSnapshot, max_chars: usize) -> String {
    if let Some(t) = not_ready(s) {
        return t;
    }
    if s.recent_trades.is_empty() {
        return format!("{}\nno closed trades yet", s.prefix);
    }
    let mut trades: Vec<&TradeRecord> = s.recent_trades.iter().collect();
    // Newest first (stable: equal times keep the ledger order, reversed).
    trades.reverse();
    trades.sort_by_key(|t| std::cmp::Reverse(t.closed_ms));
    trades.truncate(TRADES_SHOWN);
    let mut o = format!(
        "{}\nlast {} closed trades (of {}), newest first\nexit time UTC | side size | entry -> exit premium | PnL | exit reason",
        s.prefix,
        trades.len(),
        s.trades_total
    );
    // Room for the "not shown" note (about 45 chars).
    let reserve = 64;
    let mut shown = 0;
    for t in &trades {
        let line = trade_line(t);
        if o.chars().count() + 1 + line.chars().count() + reserve > max_chars {
            break;
        }
        o.push('\n');
        o.push_str(&line);
        shown += 1;
    }
    if shown < trades.len() {
        o.push_str(&format!(
            "\n… {} more not shown (message size limit)",
            trades.len() - shown
        ));
    }
    clip(&o, max_chars)
}

/// The reply to an authorized message.
pub fn reply_for(cmd: Cmd, s: &StatusSnapshot, now_ms: u64) -> String {
    match cmd {
        Cmd::Pnl => format_pnl(s, now_ms),
        Cmd::Trades => format_trades(s, MAX_ALERT_CHARS),
        Cmd::Help => help_text(if s.prefix.is_empty() {
            "[antbot]"
        } else {
            &s.prefix
        }),
    }
}

// ---------------------------------------------------------------------------
// Backoff, redaction
// ---------------------------------------------------------------------------

/// Exponential backoff: `initial`, doubling, capped at `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            next: initial,
        }
    }

    /// Delay before the next attempt (then doubles, up to the cap).
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next.min(self.max);
        self.next = self.next.saturating_mul(2).min(self.max);
        d
    }

    pub fn reset(&mut self) {
        self.next = self.initial;
    }
}

/// Remove every occurrence of the token (defence in depth on top of
/// `reqwest::Error::without_url`).
pub fn redact(s: &str, token: &str) -> String {
    if token.is_empty() {
        return s.to_string();
    }
    s.replace(token, "<redacted>")
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A failed Bot API call (message already redacted).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub message: String,
    /// `retry_after` of a 429 answer.
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The two Bot API calls the command task needs (mockable).
pub trait BotApi: Send + Sync + 'static {
    /// `getUpdates` long poll: the raw updates.
    fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> impl Future<Output = Result<Vec<Value>, ApiError>> + Send;

    /// `sendMessage` to the configured chat (plain text).
    fn send_message(&self, text: String) -> impl Future<Output = Result<(), ApiError>> + Send;
}

/// The real Bot API over HTTPS.
pub struct HttpBotApi {
    http: reqwest::Client,
    base: String,
    token: String,
    chat_id: String,
    send_timeout: Duration,
}

impl HttpBotApi {
    /// The client timeout is the long-poll timeout plus a margin.
    pub fn new(cfg: &TelegramConfig, long_poll_secs: u64) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(long_poll_secs + 10))
            .user_agent(concat!("antbot/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| redact(&error_chain(&e.without_url()), &cfg.token))?;
        Ok(Self {
            http,
            base: format!("{}/bot{}", cfg.api_base, cfg.token),
            token: cfg.token.clone(),
            chat_id: cfg.chat_id.clone(),
            send_timeout: Duration::from_secs(10),
        })
    }

    fn err(&self, msg: String, retry_after: Option<Duration>) -> ApiError {
        ApiError {
            message: redact(&msg, &self.token),
            retry_after,
        }
    }

    async fn call(
        &self,
        method: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, ApiError> {
        let mut req = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req.send().await.map_err(|e| {
            // The URL holds the token: strip it, then redact anyway.
            self.err(format!("{method}: {}", error_chain(&e.without_url())), None)
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| {
            self.err(
                format!("{method}: reading body: {}", error_chain(&e.without_url())),
                None,
            )
        })?;
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        let ok = parsed
            .as_ref()
            .and_then(|v| v.get("ok"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if status.is_success() && ok {
            return Ok(parsed
                .and_then(|mut v| v.get_mut("result").map(Value::take))
                .unwrap_or(Value::Null));
        }
        let desc = parsed
            .as_ref()
            .and_then(|v| v.get("description"))
            .and_then(Value::as_str)
            .unwrap_or(if parsed.is_none() { "invalid JSON" } else { "" });
        // Redact before cutting: a token straddling the cut would survive.
        let desc = redact(desc, &self.token)
            .chars()
            .take(200)
            .collect::<String>();
        let retry_after = parsed
            .as_ref()
            .and_then(|v| v.pointer("/parameters/retry_after"))
            .and_then(Value::as_u64)
            .map(Duration::from_secs);
        let hint = if status.as_u16() == 409 {
            " (another getUpdates consumer or a webhook is active for this bot)"
        } else {
            ""
        };
        Err(self.err(format!("{method}: HTTP {status} {desc}{hint}"), retry_after))
    }
}

impl BotApi for HttpBotApi {
    async fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>, ApiError> {
        let body = serde_json::json!({
            "offset": offset,
            "timeout": timeout_secs,
            "allowed_updates": ["message"],
        });
        match self.call("getUpdates", body, None).await? {
            Value::Array(v) => Ok(v),
            _ => Err(self.err("getUpdates: result is not an array".into(), None)),
        }
    }

    async fn send_message(&self, text: String) -> Result<(), ApiError> {
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        self.call("sendMessage", body, Some(self.send_timeout))
            .await
            .map(|_| ())
    }
}

/// An error with its cause chain.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
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

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// Poll loop settings.
#[derive(Debug, Clone)]
pub struct CommandSettings {
    /// Configured `TELEGRAM_CHAT_ID` (a private chat: the user's id).
    pub chat_id: i64,
    pub long_poll_secs: u64,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// Minimum gap between two replies.
    pub reply_gap: Duration,
    /// Delay before the poll loop is restarted after a panic.
    pub restart_delay: Duration,
    /// Messages dated before this (Unix seconds) are the backlog from
    /// before startup: ignored.
    pub min_date: i64,
    /// Minimum gap between two WARN lines for failed polls (the others
    /// are logged at debug level).
    pub warn_gap: Duration,
    /// After this long without any update the offset is reset to 0
    /// (Telegram may restart `update_id` at a random value after a week
    /// without updates; everything before was already confirmed).
    pub idle_offset_reset: Duration,
}

impl CommandSettings {
    pub fn new(chat_id: i64, start_unix_secs: i64) -> Self {
        Self {
            chat_id,
            long_poll_secs: LONG_POLL_SECS,
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            reply_gap: Duration::from_millis(1100),
            restart_delay: Duration::from_secs(30),
            // A few seconds of slack for clock skew.
            min_date: start_unix_secs - 5,
            warn_gap: Duration::from_secs(60),
            idle_offset_reset: Duration::from_secs(6 * 86_400),
        }
    }
}

/// Counters (tests and the shutdown log).
#[derive(Debug, Default)]
pub struct CommandStats {
    pub polls_ok: AtomicU64,
    pub poll_errors: AtomicU64,
    pub ignored: AtomicU64,
    pub handled: AtomicU64,
    pub reply_errors: AtomicU64,
    pub restarts: AtomicU64,
}

/// Running command task. Dropping it also stops the task.
pub struct CommandsHandle {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
    pub stats: Arc<CommandStats>,
    offset: Arc<AtomicI64>,
}

/// Why commands are off (for the startup log line), or the chat id.
pub fn commands_chat_id(
    commands_enabled: bool,
    telegram_enabled: bool,
    cfg: Option<&TelegramConfig>,
) -> Result<i64, &'static str> {
    if !commands_enabled {
        return Err("monitor.telegram_commands_enabled = false");
    }
    if !telegram_enabled {
        return Err("monitor.telegram_enabled = false");
    }
    let Some(cfg) = cfg else {
        return Err("TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID not set");
    };
    match cfg.chat_id.parse::<i64>() {
        Ok(id) if id > 0 => Ok(id),
        Ok(_) => Err(
            "TELEGRAM_CHAT_ID is a group / channel id; commands need a private chat (your user id)",
        ),
        Err(_) => Err("TELEGRAM_CHAT_ID is not a numeric user id"),
    }
}

impl CommandsHandle {
    /// Build from config + environment and start the task if enabled.
    /// Logs once whether commands are on. Must run inside a tokio runtime.
    pub fn from_env(
        commands_enabled: bool,
        telegram_enabled: bool,
        env: impl Fn(&str) -> Option<String>,
        status: watch::Receiver<Arc<StatusSnapshot>>,
    ) -> Option<Self> {
        let cfg = TelegramConfig::from_env(env);
        let chat_id = match commands_chat_id(commands_enabled, telegram_enabled, cfg.as_ref()) {
            Ok(id) => id,
            Err(why) => {
                tracing::info!("telegram commands disabled: {why}");
                return None;
            }
        };
        let cfg = cfg.expect("checked above");
        let settings = CommandSettings::new(chat_id, (crate::hl::market::now_ms() / 1000) as i64);
        let api = match HttpBotApi::new(&cfg, settings.long_poll_secs) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "telegram commands disabled: cannot build the HTTP client");
                return None;
            }
        };
        tracing::info!(
            chat_id,
            commands = "/pnl /trades /help",
            "telegram commands enabled (read-only, private chat only)"
        );
        Some(Self::spawn(api, settings, status))
    }

    /// Start the supervised poll loop.
    pub fn spawn<A: BotApi>(
        api: A,
        settings: CommandSettings,
        status: watch::Receiver<Arc<StatusSnapshot>>,
    ) -> Self {
        let (stop_tx, stop_rx) = watch::channel(false);
        let stats = Arc::new(CommandStats::default());
        let offset = Arc::new(AtomicI64::new(0));
        let api = Arc::new(api);
        let join = tokio::spawn(supervise(
            api,
            settings,
            status,
            stop_rx,
            stats.clone(),
            offset.clone(),
        ));
        Self {
            stop: stop_tx,
            join,
            stats,
            offset,
        }
    }

    /// Next `getUpdates` offset (last seen `update_id` + 1).
    pub fn offset(&self) -> i64 {
        self.offset.load(Ordering::Relaxed)
    }

    /// Ask the task to stop without waiting (see [`Self::shutdown`]).
    pub fn request_stop(&self) {
        let _ = self.stop.send(true);
    }

    /// Stop the task: waits at most `timeout`, then aborts it.
    pub async fn shutdown(self, timeout: Duration) {
        let _ = self.stop.send(true);
        let mut join = self.join;
        if tokio::time::timeout(timeout, &mut join).await.is_err() {
            tracing::warn!("telegram command task did not stop in time: aborted");
            join.abort();
        }
        let s = &self.stats;
        tracing::info!(
            handled = s.handled.load(Ordering::Relaxed),
            ignored = s.ignored.load(Ordering::Relaxed),
            poll_errors = s.poll_errors.load(Ordering::Relaxed),
            reply_errors = s.reply_errors.load(Ordering::Relaxed),
            restarts = s.restarts.load(Ordering::Relaxed),
            "telegram command task stopped"
        );
    }
}

/// Resolves once a stop is requested (or the handle is gone).
async fn stopped(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Sleep unless a stop comes first. Returns `false` if stopped.
async fn sleep_or_stop(d: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        biased;
        _ = stopped(stop) => false,
        _ = tokio::time::sleep(d) => true,
    }
}

/// Startup: drop the backlog (updates sent while the bot was down) with
/// `getUpdates(offset = -1)`, which returns only the last update and
/// forgets the older ones; the next poll starts after it. Nothing is
/// answered. On failure the date filter still drops old messages.
/// Returns `false` if stopped meanwhile.
async fn skip_backlog<A: BotApi>(
    api: &A,
    offset: &AtomicI64,
    stats: &CommandStats,
    stop: &mut watch::Receiver<bool>,
) -> bool {
    let res = tokio::select! {
        biased;
        _ = stopped(stop) => return false,
        r = api.get_updates(-1, 0) => r,
    };
    match res {
        Ok(v) => {
            if let Some(last) = v.iter().filter_map(update_id).max() {
                offset.fetch_max(last.saturating_add(1), Ordering::Relaxed);
                stats.ignored.fetch_add(v.len() as u64, Ordering::Relaxed);
                tracing::info!(
                    last_update_id = last,
                    "telegram commands: backlog from before startup skipped"
                );
            }
        }
        Err(e) => tracing::debug!(error = %e, "telegram backlog skip failed (date filter applies)"),
    }
    true
}

/// Run the poll loop; restart it (after a delay) if it panics.
async fn supervise<A: BotApi>(
    api: Arc<A>,
    settings: CommandSettings,
    status: watch::Receiver<Arc<StatusSnapshot>>,
    mut stop: watch::Receiver<bool>,
    stats: Arc<CommandStats>,
    offset: Arc<AtomicI64>,
) {
    loop {
        let run = tokio::spawn(poll_loop(
            api.clone(),
            settings.clone(),
            status.clone(),
            stop.clone(),
            stats.clone(),
            offset.clone(),
        ));
        let abort = run.abort_handle();
        let res = tokio::select! {
            r = run => r,
            _ = stopped(&mut stop) => {
                // The loop watches the same signal; give it a moment.
                abort.abort();
                return;
            }
        };
        match res {
            Ok(()) => return,
            Err(e) if e.is_panic() => {
                stats.restarts.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    restart_in = ?settings.restart_delay,
                    "telegram command task panicked (trading unaffected); restarting"
                );
                if !sleep_or_stop(settings.restart_delay, &mut stop).await {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

async fn poll_loop<A: BotApi>(
    api: Arc<A>,
    s: CommandSettings,
    status: watch::Receiver<Arc<StatusSnapshot>>,
    mut stop: watch::Receiver<bool>,
    stats: Arc<CommandStats>,
    offset: Arc<AtomicI64>,
) {
    let mut backoff = Backoff::new(s.backoff_initial, s.backoff_max);
    let mut last_reply: Option<Instant> = None;
    let mut last_warn: Option<Instant> = None;
    let mut last_update_at = Instant::now();
    if offset.load(Ordering::Relaxed) == 0 && !skip_backlog(&*api, &offset, &stats, &mut stop).await
    {
        return;
    }
    loop {
        if last_update_at.elapsed() >= s.idle_offset_reset {
            last_update_at = Instant::now();
            if offset.swap(0, Ordering::Relaxed) != 0 {
                tracing::info!("telegram commands: no update for a long time, offset reset");
            }
        }
        let off = offset.load(Ordering::Relaxed);
        let res = tokio::select! {
            biased;
            _ = stopped(&mut stop) => return,
            r = api.get_updates(off, s.long_poll_secs) => r,
        };
        let updates = match res {
            Ok(u) => {
                backoff.reset();
                stats.polls_ok.fetch_add(1, Ordering::Relaxed);
                u
            }
            Err(e) => {
                stats.poll_errors.fetch_add(1, Ordering::Relaxed);
                let wait = backoff
                    .next_delay()
                    .max(e.retry_after.unwrap_or_default())
                    .min(s.backoff_max);
                if last_warn.is_none_or(|t| t.elapsed() >= s.warn_gap) {
                    last_warn = Some(Instant::now());
                    tracing::warn!(error = %e, retry_in = ?wait, "telegram getUpdates failed (commands only, trading unaffected)");
                } else {
                    tracing::debug!(error = %e, retry_in = ?wait, "telegram getUpdates failed");
                }
                if !sleep_or_stop(wait, &mut stop).await {
                    return;
                }
                continue;
            }
        };
        if !updates.is_empty() {
            last_update_at = Instant::now();
        }
        for u in updates {
            let Some(id) = update_id(&u) else {
                stats.ignored.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("telegram update without update_id ignored");
                continue;
            };
            // Advance first: an update that breaks the handler is skipped
            // after a restart.
            offset.fetch_max(id.saturating_add(1), Ordering::Relaxed);
            let text = match authorize(&u, s.chat_id, s.min_date) {
                Decision::Accept(t) => t,
                Decision::Ignore(why) => {
                    stats.ignored.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(update_id = id, reason = why, "telegram update ignored");
                    continue;
                }
            };
            let cmd = parse_command(&text);
            // Clone the Arc and release the watch lock at once.
            let snap = status.borrow().clone();
            let reply = reply_for(cmd, &snap, crate::hl::market::now_ms());
            drop(snap);
            if let Some(t) = last_reply {
                let at = t + s.reply_gap;
                if at > Instant::now() {
                    let wait = at - Instant::now();
                    if !sleep_or_stop(wait, &mut stop).await {
                        return;
                    }
                }
            }
            tracing::info!(command = cmd.as_str(), "telegram command");
            let mut sent = tokio::select! {
                biased;
                _ = stopped(&mut stop) => return,
                r = api.send_message(reply.clone()) => r,
            };
            // Rate limited (429): honour retry_after once.
            if let Some(wait) = sent.as_ref().err().and_then(|e| e.retry_after) {
                if !sleep_or_stop(wait.min(s.backoff_max), &mut stop).await {
                    return;
                }
                sent = tokio::select! {
                    biased;
                    _ = stopped(&mut stop) => return,
                    r = api.send_message(reply) => r,
                };
            }
            last_reply = Some(Instant::now());
            stats.handled.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = sent {
                stats.reply_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e, command = cmd.as_str(), "telegram command reply not delivered");
            }
        }
    }
}

#[cfg(test)]
mod tests;
