use super::*;

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::json;

use crate::config::Config;
use crate::engine::EngineParams;
use crate::exec::state::Ledger;
use crate::risk::{RiskManager, RiskParams, RiskState};
use crate::strategy::signal::{ExitReason, Strategy, StrategyParams};

const ME: i64 = 424_242;
const START: i64 = 1_800_000_000;
/// 2027-01-15T08:00:00Z
const T0: u64 = 1_800_000_000_000;

fn msg(chat_id: i64, chat_type: &str, from: Option<i64>, text: &str) -> Value {
    let mut m = json!({
        "message_id": 7,
        "date": START + 10,
        "chat": {"id": chat_id, "type": chat_type},
        "text": text,
    });
    if let Some(f) = from {
        m["from"] = json!({"id": f, "is_bot": false, "first_name": "op"});
    }
    json!({"update_id": 100, "message": m})
}

fn ok_msg(text: &str) -> Value {
    msg(ME, "private", Some(ME), text)
}

fn auth(u: &Value) -> Decision {
    authorize(u, ME, START - 5)
}

#[test]
fn authorization_filter() {
    assert_eq!(auth(&ok_msg("/pnl")), Decision::Accept("/pnl".into()));
    // Wrong chat id (private chat with someone else).
    assert_eq!(
        auth(&msg(ME + 1, "private", Some(ME + 1), "/pnl")),
        Decision::Ignore("wrong chat id")
    );
    // Right chat, wrong sender (cannot happen in a private chat, but the
    // check is independent).
    assert_eq!(
        auth(&msg(ME, "private", Some(ME + 1), "/pnl")),
        Decision::Ignore("wrong sender id")
    );
    // Sender in the right chat id but the chat is not private.
    assert_eq!(
        auth(&msg(-100_123, "group", Some(ME), "/pnl")),
        Decision::Ignore("not a private chat")
    );
    for t in ["group", "supergroup", "channel"] {
        assert_eq!(
            auth(&msg(ME, t, Some(ME), "/pnl")),
            Decision::Ignore("not a private chat"),
            "{t}"
        );
    }
    // Missing `from`.
    assert_eq!(
        auth(&msg(ME, "private", None, "/pnl")),
        Decision::Ignore("no sender")
    );
    // A bot as sender.
    let mut u = ok_msg("/pnl");
    u["message"]["from"]["is_bot"] = json!(true);
    assert_eq!(auth(&u), Decision::Ignore("sender is a bot"));
    // Other update kinds, even from the right chat.
    let inner = ok_msg("/pnl")["message"].clone();
    for (k, why) in [
        ("edited_message", "edited message"),
        ("channel_post", "channel post"),
        ("edited_channel_post", "edited channel post"),
    ] {
        assert_eq!(
            auth(&json!({"update_id": 1, k: inner.clone()})),
            Decision::Ignore(why)
        );
    }
    let cb = json!({"update_id": 1, "callback_query": {"id": "1", "from": {"id": ME, "is_bot": false}, "data": "pnl"}});
    assert_eq!(auth(&cb), Decision::Ignore("callback query"));
    assert_eq!(
        auth(&json!({"update_id": 1, "my_chat_member": {}})),
        Decision::Ignore("not a message")
    );
    // Malformed message (chat id as a string).
    let mut u = ok_msg("/pnl");
    u["message"]["chat"]["id"] = json!("x");
    assert_eq!(auth(&u), Decision::Ignore("malformed message"));
    // Non-text message.
    let mut u = ok_msg("/pnl");
    u["message"].as_object_mut().unwrap().remove("text");
    u["message"]["sticker"] = json!({});
    assert_eq!(auth(&u), Decision::Ignore("not a text message"));
    // Backlog from before startup.
    let mut u = ok_msg("/pnl");
    u["message"]["date"] = json!(START - 3600);
    assert_eq!(auth(&u), Decision::Ignore("sent before startup"));
    // Within the clock-skew slack.
    u["message"]["date"] = json!(START - 5);
    assert_eq!(auth(&u), Decision::Accept("/pnl".into()));
}

#[test]
fn command_parsing() {
    for t in [
        "/pnl",
        "/PNL",
        "/Pnl",
        "/pnl@antbot_bot",
        "/PnL@AntBot_Bot",
        "/pnl now please",
        "  /pnl",
        "/pnl@bot extra",
    ] {
        assert_eq!(parse_command(t), Cmd::Pnl, "{t}");
    }
    for t in ["/trades", "/TRADES@antbot_bot", "/trades 50"] {
        assert_eq!(parse_command(t), Cmd::Trades, "{t}");
    }
    for t in [
        "/help",
        "/start",
        "/unknown",
        "/pnlx",
        "/pn",
        "hello",
        "pnl",
        "",
        "/",
        "/@bot",
        "@bot /pnl",
        "/ pnl",
    ] {
        assert_eq!(parse_command(t), Cmd::Help, "{t:?}");
    }
}

fn trade(closed_ms: u64, i: usize) -> TradeRecord {
    TradeRecord {
        side: if i % 2 == 0 { Side::Short } else { Side::Long },
        size: 24.5,
        entry_px: 101.25,
        exit_px: 101.0,
        signal_ms: closed_ms - 3_000_000,
        opened_ms: closed_ms - 2_700_000,
        closed_ms,
        entry_premium_bps: Some(62.14),
        exit_premium_bps: Some(3.2),
        fees_usd: 0.25,
        funding_usd: 0.0,
        gross_pnl_usd: 6.0,
        pnl_usd: 5.75 + i as f64,
        pnl_bps: 23.0,
        exit_reason: ExitReason::Converged,
        exit_taker_sz: 0.0,
    }
}

fn snap() -> StatusSnapshot {
    StatusSnapshot {
        prefix: "[antbot paper xyz:ANT]".into(),
        as_of_ms: T0 + 5_000,
        phase: "flat".into(),
        premium_bps: Some(12.345),
        raw_premium_bps: Some(10.1),
        realized_total_usd: 12.344,
        realized_today_usd: -3.2,
        trades_today: 2,
        equity_usd: 12.344,
        fees_paid_usd: 4.5678,
        trades_total: 17,
        ..StatusSnapshot::default()
    }
}

#[test]
fn pnl_flat() {
    let s = snap();
    assert_eq!(
        format_pnl(&s, T0 + 6_000),
        "[antbot paper xyz:ANT]\nstatus as of 2027-01-15 08:00:05 UTC\nphase: flat\nposition: flat\npremium: signal +12.3 bps (raw +10.1 bps)\nrealized PnL: +12.34 USD total, -3.20 USD today (UTC, 2 trades)\nunrealized PnL: +0.00 USD\nequity (realized + unrealized): +12.34 USD\nfees paid: 4.5678 USD\ntrades: 17\nkill switch: not active"
    );
}

#[test]
fn pnl_with_position_and_kill() {
    let mut s = snap();
    s.phase = "in_position".into();
    s.position = Some(PositionInfo {
        side: Side::Short,
        size: 24.5,
        entry_px: Some(101.23456),
        entry_premium_bps: Some(62.14),
        opened_ms: T0 - 83 * 60_000,
    });
    s.premium_stale = true;
    s.unrealized_usd = -1.234;
    s.venue_position = Some(-24.5);
    s.premium_ready = true;
    s.trades_today = 1;
    s.kill = Some(KillInfo {
        rule: "daily_loss".into(),
        at_ms: T0 - 60_000,
        message: "daily loss -151.20 USD <= -150 USD".into(),
    });
    let t = format_pnl(&s, T0);
    assert!(t.contains("\nphase: in_position\n"), "{t}");
    assert!(
        t.contains("\nposition: SHORT 24.5 @ 101.2346, held 1h23m\n"),
        "{t}"
    );
    assert!(
        t.contains("\npremium: entry +62.1 bps, signal +12.3 bps (raw +10.1 bps, data stale)\n"),
        "{t}"
    );
    // Account agrees with the strategy: not shown.
    assert!(!t.contains("account position"), "{t}");
    assert!(t.contains("today (UTC, 1 trade)\n"), "{t}");
    assert!(
        t.contains("\nunrealized PnL: -1.23 USD (net of fees / funding so far)\n"),
        "{t}"
    );
    assert!(t.ends_with(
        "\n⛔ kill switch ACTIVE (daily_loss) since 2027-01-15 07:59:00 UTC: daily loss -151.20 USD <= -150 USD"
    ), "{t}");
    // Account disagrees: flagged.
    s.venue_position = Some(0.0);
    let t = format_pnl(&s, T0);
    assert!(
        t.contains("\n⚠️ account position 0 differs from the strategy position -24.5\n"),
        "{t}"
    );
    // Signal not available yet (averaging warm-up).
    s.premium_bps = None;
    s.premium_ready = false;
    assert!(format_pnl(&s, T0).contains("signal n/a (averaging window warming up) (raw +10.1 bps"));
    s.premium_ready = true;
    assert!(format_pnl(&s, T0).contains("signal n/a (raw +10.1 bps"));
    // A huge kill message is cut under the limit.
    s.kill.as_mut().unwrap().message = "x".repeat(10_000);
    let t = format_pnl(&s, T0);
    assert!(t.chars().count() <= MAX_ALERT_CHARS);
    assert!(t.ends_with("(truncated)"));
}

#[test]
fn not_ready_before_first_snapshot() {
    let s = StatusSnapshot::default();
    for c in [Cmd::Pnl, Cmd::Trades] {
        assert!(reply_for(c, &s, T0).contains("state not available yet"));
    }
    assert!(reply_for(Cmd::Help, &s, T0).contains("/pnl"));
}

#[test]
fn help_lists_commands() {
    let h = help_text("[antbot paper xyz:ANT]");
    assert!(h.starts_with("[antbot paper xyz:ANT]\n"));
    for c in ["/pnl", "/trades", "/help"] {
        assert!(h.contains(c), "{c}");
    }
    assert!(h.contains("never change"));
}

#[test]
fn trades_empty() {
    assert_eq!(
        format_trades(&snap(), MAX_ALERT_CHARS),
        "[antbot paper xyz:ANT]\nno closed trades yet"
    );
}

#[test]
fn trades_newest_first_max_20() {
    let mut s = snap();
    // 25 trades in the ledger: the snapshot keeps the last 20 (chronological).
    let all: Vec<TradeRecord> = (0..25)
        .map(|i| trade(T0 + i as u64 * 3_600_000, i))
        .collect();
    s.recent_trades = all[5..].to_vec();
    s.trades_total = 25;
    let t = format_trades(&s, MAX_ALERT_CHARS);
    assert!(t.chars().count() <= MAX_ALERT_CHARS);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[1], "last 20 closed trades (of 25), newest first");
    let rows = &lines[3..];
    assert_eq!(rows.len(), 20);
    // Newest (i = 24, 2027-01-16 08:00) first, oldest shown is i = 5.
    assert_eq!(
        rows[0],
        "2027-01-16 08:00 SHORT 24.5 | prem +62.1 -> +3.2 bps | +29.75 USD | converged"
    );
    assert!(
        rows[19].starts_with("2027-01-15 13:00 LONG "),
        "{}",
        rows[19]
    );
    assert!(rows[19].contains("+10.75 USD"));
    assert!(!t.contains("not shown"));
}

#[test]
fn trades_order_does_not_depend_on_ledger_order() {
    let mut s = snap();
    s.recent_trades = vec![trade(T0 + 2_000, 2), trade(T0, 0), trade(T0 + 1_000, 1)];
    let t = format_trades(&s, MAX_ALERT_CHARS);
    let pnl: Vec<&str> = t
        .lines()
        .skip(3)
        .map(|l| l.split(" | ").nth(2).unwrap())
        .collect();
    assert_eq!(pnl, ["+7.75 USD", "+6.75 USD", "+5.75 USD"]);
}

#[test]
fn trades_truncated_under_the_limit() {
    let mut s = snap();
    s.recent_trades = (0..20).map(|i| trade(T0 + i as u64, i)).collect();
    s.trades_total = 20;
    let t = format_trades(&s, 700);
    assert!(t.chars().count() <= 700, "{}", t.chars().count());
    let shown = t.lines().filter(|l| l.contains(" USD | ")).count();
    assert!(shown > 0 && shown < 20);
    assert!(
        t.ends_with(&format!(
            "… {} more not shown (message size limit)",
            20 - shown
        )),
        "{t}"
    );
    // Extreme sizes still fit Telegram's 4096 limit at the default budget.
    for tr in &mut s.recent_trades {
        tr.size = 123_456_789.123_456_79;
        tr.entry_premium_bps = Some(-1.0e12);
    }
    let t = format_trades(&s, MAX_ALERT_CHARS);
    assert!(t.chars().count() <= MAX_ALERT_CHARS && MAX_ALERT_CHARS < 4096);
}

#[test]
fn snapshot_from_engine() {
    let cfg = Config::default();
    let mut ledger = Ledger::default();
    // Yesterday, then two trades today.
    for (i, at) in [T0 - 86_400_000, T0 - 60_000, T0 - 30_000]
        .into_iter()
        .enumerate()
    {
        ledger.record_trade(&trade(at, i));
    }
    let mut e = Engine::new(
        EngineParams::from_config(&cfg),
        Strategy::new(StrategyParams::from_config(&cfg, 2, 10)),
        RiskManager::new(RiskParams::from(&cfg.risk), RiskState::default()),
        ledger,
        0,
    );
    let flat = StatusSnapshot::from_engine(&e, "[p]", T0, None);
    assert_eq!(flat.phase, "flat");
    assert_eq!(flat.position, None);
    assert_eq!(flat.trades_total, 3);
    assert_eq!(flat.trades_today, 2);
    assert!((flat.realized_today_usd - (6.75 + 7.75)).abs() < 1e-9);
    assert!((flat.realized_total_usd - (5.75 + 6.75 + 7.75)).abs() < 1e-9);
    assert!((flat.fees_paid_usd - 0.75).abs() < 1e-9);
    assert_eq!(flat.recent_trades.len(), 3);
    assert_eq!(flat.kill, None);
    assert!(flat.premium_stale);

    e.strategy
        .adopt_position(Side::Long, 10.0, 100.0, T0)
        .unwrap();
    e.manual_kill_with(T0, "operator test", false);
    let s = StatusSnapshot::from_engine(&e, "[p]", T0 + 1, Some(10.0));
    let p = s.position.unwrap();
    assert_eq!(
        (p.side, p.size, p.entry_px),
        (Side::Long, 10.0, Some(100.0))
    );
    assert_eq!(s.venue_position, Some(10.0));
    let k = s.kill.unwrap();
    assert_eq!(k.rule, "manual");
    assert!(k.message.contains("operator test"), "{}", k.message);
}

#[test]
fn snapshot_keeps_last_20_trades() {
    let cfg = Config::default();
    let mut ledger = Ledger::default();
    for i in 0..30 {
        ledger.record_trade(&trade(T0 + i as u64, i));
    }
    let e = Engine::new(
        EngineParams::from_config(&cfg),
        Strategy::new(StrategyParams::from_config(&cfg, 2, 10)),
        RiskManager::new(RiskParams::from(&cfg.risk), RiskState::default()),
        ledger,
        0,
    );
    let s = StatusSnapshot::from_engine(&e, "[p]", T0, None);
    assert_eq!(s.recent_trades.len(), TRADES_SHOWN);
    assert_eq!(s.recent_trades.last().unwrap().closed_ms, T0 + 29);
    assert_eq!(s.recent_trades[0].closed_ms, T0 + 10);
    assert_eq!(s.trades_total, 30);
}

#[test]
fn backoff_doubles_to_cap_and_resets() {
    let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
    let got: Vec<u64> = (0..9).map(|_| b.next_delay().as_secs()).collect();
    assert_eq!(got, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    b.reset();
    assert_eq!(b.next_delay(), Duration::from_secs(1));
}

#[test]
fn token_redaction() {
    let tok = "123456:ABC-secret";
    assert_eq!(
        redact("error sending request for url (https://api.telegram.org/bot123456:ABC-secret/getUpdates)", tok),
        "error sending request for url (https://api.telegram.org/bot<redacted>/getUpdates)"
    );
    assert_eq!(redact("x", ""), "x");
}

#[test]
fn enablement() {
    let cfg = |chat: &str| TelegramConfig {
        token: "t".into(),
        chat_id: chat.into(),
        api_base: "http://x".into(),
    };
    assert_eq!(commands_chat_id(true, true, Some(&cfg("42"))), Ok(42));
    assert!(commands_chat_id(false, true, Some(&cfg("42"))).is_err());
    assert!(commands_chat_id(true, false, Some(&cfg("42"))).is_err());
    assert!(commands_chat_id(true, true, None).is_err());
    assert!(commands_chat_id(true, true, Some(&cfg("-100123")))
        .unwrap_err()
        .contains("group"));
    assert!(commands_chat_id(true, true, Some(&cfg("@channel")))
        .unwrap_err()
        .contains("numeric"));
    // Disabled: no runtime needed, nothing spawned.
    let (_tx, rx) = watch::channel(Arc::new(StatusSnapshot::default()));
    assert!(CommandsHandle::from_env(true, true, |_| None, rx).is_none());
}

// ---------------------------------------------------------------------------
// Poll loop against a fake transport (paused tokio time)
// ---------------------------------------------------------------------------

type Poll = Result<Vec<Value>, ApiError>;

#[derive(Default)]
struct Fake {
    /// Scripted getUpdates answers; when empty the call pends forever.
    polls: Mutex<VecDeque<Poll>>,
    /// Offsets requested.
    offsets: Mutex<Vec<i64>>,
    /// Time of each getUpdates call.
    poll_at: Mutex<Vec<Instant>>,
    sent: Mutex<Vec<String>>,
    /// Panic on the n-th sendMessage (1-based).
    panic_on_send: Option<usize>,
    /// Scripted sendMessage answers (then Ok).
    send_results: Mutex<VecDeque<Result<(), ApiError>>>,
    sends: AtomicU64,
    /// Answer of the startup backlog skip (`offset = -1`).
    backlog: Mutex<Vec<Value>>,
    backlog_calls: AtomicU64,
    /// Delay of each scripted getUpdates answer.
    poll_delay: Duration,
}

impl BotApi for Arc<Fake> {
    async fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>, ApiError> {
        if offset == -1 {
            assert_eq!(timeout_secs, 0);
            self.backlog_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(self.backlog.lock().unwrap().clone());
        }
        if !self.poll_delay.is_zero() {
            tokio::time::sleep(self.poll_delay).await;
        }
        self.offsets.lock().unwrap().push(offset);
        self.poll_at.lock().unwrap().push(Instant::now());
        let next = self.polls.lock().unwrap().pop_front();
        match next {
            Some(r) => r,
            None => std::future::pending().await,
        }
    }

    async fn send_message(&self, text: String) -> Result<(), ApiError> {
        let n = self.sends.fetch_add(1, Ordering::SeqCst) as usize + 1;
        if self.panic_on_send == Some(n) {
            panic!("injected panic in sendMessage");
        }
        let r = self.send_results.lock().unwrap().pop_front();
        if let Some(Err(e)) = r {
            return Err(e);
        }
        self.sent.lock().unwrap().push(text);
        Ok(())
    }
}

fn with_id(mut u: Value, id: i64) -> Value {
    u["update_id"] = json!(id);
    u
}

fn api_err(m: &str) -> ApiError {
    ApiError {
        message: m.into(),
        retry_after: None,
    }
}

fn settings() -> CommandSettings {
    CommandSettings::new(ME, START)
}

fn status() -> (
    watch::Sender<Arc<StatusSnapshot>>,
    watch::Receiver<Arc<StatusSnapshot>>,
) {
    let mut s = snap();
    s.recent_trades = vec![trade(T0, 0)];
    watch::channel(Arc::new(s))
}

/// Let the spawned tasks run until they block (paused time auto-advances
/// only when every task is idle).
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn offset_advances_past_ignored_and_malformed_updates() {
    let fake = Arc::new(Fake::default());
    fake.polls.lock().unwrap().extend([
        Ok(vec![
            with_id(ok_msg("/pnl@antbot_bot"), 10),
            with_id(msg(ME + 1, "private", Some(ME + 1), "/pnl"), 11),
            json!({"update_id": 12, "message": {"bogus": true}}),
            with_id(msg(-1_005, "supergroup", Some(ME), "/pnl"), 13),
            json!({"no_id": true}),
        ]),
        Ok(vec![]),
        Ok(vec![
            with_id(ok_msg("/trades"), 14),
            with_id(ok_msg("hi"), 15),
        ]),
    ]);
    let (_tx, rx) = status();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(*fake.offsets.lock().unwrap(), [0, 14, 14, 16]);
    assert_eq!(h.offset(), 16);
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[0].contains("realized PnL"));
    assert!(sent[1].contains("last 1 closed trades"));
    assert!(sent[2].contains("Read-only commands"));
    assert_eq!(h.stats.ignored.load(Ordering::Relaxed), 4);
    assert_eq!(h.stats.handled.load(Ordering::Relaxed), 3);
    h.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn replies_are_paced() {
    let fake = Arc::new(Fake::default());
    fake.polls.lock().unwrap().push_back(Ok(vec![
        with_id(ok_msg("/pnl"), 1),
        with_id(ok_msg("/pnl"), 2),
        with_id(ok_msg("/pnl"), 3),
    ]));
    let (_tx, rx) = status();
    let t0 = Instant::now();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    settle().await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(fake.sent.lock().unwrap().len(), 1);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(fake.sent.lock().unwrap().len(), 3);
    assert!(Instant::now() - t0 >= Duration::from_millis(2200));
    h.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn errors_back_off_exponentially_then_reset() {
    let fake = Arc::new(Fake::default());
    fake.polls.lock().unwrap().extend([
        Err(api_err("HTTP 502")),
        Err(api_err("HTTP 409 Conflict")),
        Err(api_err("timeout")),
        Ok(vec![]),
        Err(api_err("HTTP 500")),
        Err(ApiError {
            message: "HTTP 429".into(),
            retry_after: Some(Duration::from_secs(7)),
        }),
    ]);
    let (_tx, rx) = status();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    tokio::time::sleep(Duration::from_secs(120)).await;
    let at = fake.poll_at.lock().unwrap().clone();
    let gaps: Vec<u64> = at.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    // 1, 2, 4 s after the three errors; the success resets (immediate
    // next poll), 1 s after the 500, then max(2 s, retry_after 7 s).
    assert_eq!(gaps, [1, 2, 4, 0, 1, 7]);
    assert_eq!(h.stats.poll_errors.load(Ordering::Relaxed), 5);
    h.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn backoff_is_capped_at_60s() {
    let fake = Arc::new(Fake::default());
    fake.polls
        .lock()
        .unwrap()
        .extend((0..9).map(|_| Err(api_err("down"))));
    // A huge retry_after is capped too.
    fake.polls.lock().unwrap().push_back(Err(ApiError {
        message: "HTTP 429".into(),
        retry_after: Some(Duration::from_secs(3600)),
    }));
    let (_tx, rx) = status();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    tokio::time::sleep(Duration::from_secs(700)).await;
    let at = fake.poll_at.lock().unwrap().clone();
    let gaps: Vec<u64> = at.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    assert_eq!(gaps, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    h.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn panic_is_contained_restarted_and_not_replayed() {
    let fake = Arc::new(Fake {
        panic_on_send: Some(1),
        ..Fake::default()
    });
    fake.polls.lock().unwrap().extend([
        Ok(vec![
            with_id(ok_msg("/pnl"), 50),
            with_id(ok_msg("/help"), 51),
        ]),
        // After the restart (offset 51): the rest of the batch again.
        Ok(vec![with_id(ok_msg("/help"), 51)]),
    ]);
    let (_tx, rx) = status();
    let mut s = settings();
    s.restart_delay = Duration::from_secs(5);
    let h = CommandsHandle::spawn(fake.clone(), s, rx);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(h.stats.restarts.load(Ordering::Relaxed), 1);
    assert!(fake.sent.lock().unwrap().is_empty());
    tokio::time::sleep(Duration::from_secs(5)).await;
    // The update that panicked (50) is not requested again.
    assert_eq!(*fake.offsets.lock().unwrap(), [0, 51, 52]);
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("Read-only commands"));
    h.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_long_poll_and_backoff() {
    // Pending long poll.
    let fake = Arc::new(Fake::default());
    let (_tx, rx) = status();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    settle().await;
    assert_eq!(fake.offsets.lock().unwrap().len(), 1);
    let t0 = Instant::now();
    let join_done = {
        let stop = h.stop.clone();
        let _ = stop.send(true);
        tokio::time::timeout(Duration::from_millis(10), h.join)
            .await
            .is_ok()
    };
    assert!(join_done);
    assert!(Instant::now() - t0 < Duration::from_millis(10));

    // In a 60 s backoff sleep.
    let fake = Arc::new(Fake::default());
    fake.polls
        .lock()
        .unwrap()
        .extend((0..7).map(|_| Err(api_err("down"))));
    let (_tx, rx) = status();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    tokio::time::sleep(Duration::from_secs(70)).await;
    let t0 = Instant::now();
    h.shutdown(Duration::from_secs(1)).await;
    assert!(Instant::now() - t0 < Duration::from_millis(10));

    // Dropping the handle stops the task as well.
    let fake = Arc::new(Fake::default());
    let (_tx, rx) = status();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    let abort = h.join.abort_handle();
    settle().await;
    drop(h);
    settle().await;
    assert!(abort.is_finished());
}

#[tokio::test(start_paused = true)]
async fn replies_use_the_latest_snapshot() {
    let fake = Arc::new(Fake::default());
    fake.polls
        .lock()
        .unwrap()
        .push_back(Ok(vec![with_id(ok_msg("/pnl"), 1)]));
    let (tx, rx) = status();
    let mut s = snap();
    s.trades_total = 99;
    tx.send_replace(Arc::new(s));
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    settle().await;
    assert!(fake.sent.lock().unwrap()[0].contains("\ntrades: 99\n"));
    h.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn startup_backlog_is_skipped_without_replies() {
    let fake = Arc::new(Fake::default());
    *fake.backlog.lock().unwrap() = vec![with_id(ok_msg("/pnl"), 30)];
    fake.polls
        .lock()
        .unwrap()
        .push_back(Ok(vec![with_id(ok_msg("/help"), 31)]));
    let (_tx, rx) = status();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    settle().await;
    assert_eq!(fake.backlog_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*fake.offsets.lock().unwrap(), [31, 32]);
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("Read-only commands"));
    h.shutdown(Duration::from_secs(1)).await;

    // Empty backlog: polling starts at 0.
    let fake = Arc::new(Fake::default());
    let (_tx, rx) = status();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    settle().await;
    assert_eq!(*fake.offsets.lock().unwrap(), [0]);
    h.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn rate_limited_reply_is_retried_once() {
    let fake = Arc::new(Fake::default());
    fake.polls.lock().unwrap().push_back(Ok(vec![
        with_id(ok_msg("/pnl"), 1),
        with_id(ok_msg("/trades"), 2),
    ]));
    fake.send_results.lock().unwrap().extend([
        Err(ApiError {
            message: "HTTP 429".into(),
            retry_after: Some(Duration::from_secs(3)),
        }),
        Ok(()),
        // /trades: a plain 400 is not retried.
        Err(api_err("HTTP 400")),
    ]);
    let (_tx, rx) = status();
    let t0 = Instant::now();
    let h = CommandsHandle::spawn(fake.clone(), settings(), rx);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(fake.sends.load(Ordering::SeqCst), 3);
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("realized PnL"));
    assert_eq!(h.stats.reply_errors.load(Ordering::Relaxed), 1);
    assert!(Instant::now() - t0 >= Duration::from_secs(3));
    h.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn offset_resets_after_a_long_idle_period() {
    let fake = Arc::new(Fake {
        poll_delay: Duration::from_secs(5),
        ..Fake::default()
    });
    fake.polls.lock().unwrap().extend([
        Ok(vec![json!({"update_id": 5, "my_chat_member": {}})]),
        Ok(vec![]),
        Ok(vec![]),
        Ok(vec![]),
    ]);
    let (_tx, rx) = status();
    let mut st = settings();
    st.idle_offset_reset = Duration::from_secs(12);
    let h = CommandsHandle::spawn(fake.clone(), st, rx);
    tokio::time::sleep(Duration::from_secs(30)).await;
    // Updates at t=5; empty polls end at 10, 15, 20: at 20 the offset is
    // reset (15 s without updates).
    assert_eq!(*fake.offsets.lock().unwrap(), [0, 6, 6, 6, 0]);
    h.shutdown(Duration::from_secs(1)).await;
}

#[test]
fn numbers_never_print_negative_zero_or_noise() {
    assert_eq!(usd(-0.004), "+0.00");
    assert_eq!(usd(-0.0), "+0.00");
    assert_eq!(usd(-0.005), "-0.01");
    assert_eq!(usd(f64::NAN), "n/a");
    assert_eq!(usd(f64::INFINITY), "n/a");
    assert_eq!(bps(Some(-0.04)), "+0.0");
    assert_eq!(bps(Some(f64::NAN)), "n/a");
    assert_eq!(qty(24.500000000000004), "24.5");
    assert_eq!(qty(-0.0), "0");
    assert_eq!(qty(-24.5), "-24.5");
    // A tiny budget never panics.
    assert!(clip("abcdef", 3).ends_with("(truncated)"));
}
