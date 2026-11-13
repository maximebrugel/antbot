//! `antbot run`: the main loop.
//!
//! Wiring: websocket feed (`l2Book`, `activeAssetCtx`, plus `trades` in
//! paper mode and `userFills` / `orderUpdates` in live modes) -> executor
//! (paper fills / live order feedback) -> [`Engine`] (strategy + risk) ->
//! commands -> executor. Timers drive the time-based rules (1 s tick),
//! the spec / meta poll (kill rules), live reconciliation, state
//! persistence, premium samples and the heartbeat. SIGINT / SIGTERM stop the
//! loop gracefully: the executor cancels its open orders (live) and both
//! states are saved.
//!
//! Monitoring ([`crate::monitor`]): every order command and order feedback
//! goes to `orders.csv`, closed trades to `trades.csv`, premium samples to
//! `signals.csv`; entries, exits, kills, warnings / errors, startup /
//! shutdown and the daily summary are sent to Telegram (when configured).
//! The read-only Telegram commands (`/pnl`, `/trades`, `/help`) run in their
//! own task and only read a [`StatusSnapshot`] the loop publishes every
//! second and after entries, closed trades and kills.
//!
//! Paper mode never builds an exchange client: the executor is chosen from
//! the mode, and [`crate::hl::exchange::ExchangeClient::new`] refuses paper
//! mode anyway.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::{mpsc, watch};

use crate::config::HARD_CAP_NOTIONAL_USD;
use crate::config::{Config, Mode};
use crate::engine::{Engine, EngineParams, Notice};
use crate::exec::live::{HlApi, LiveExecutor, LiveParams};
use crate::exec::paper::{PaperExecutor, PaperParams};
use crate::exec::state::{state_path, BotState, Ledger, STATE_VERSION};
use crate::exec::{ExecEvent, Executor, OrderView};
use crate::hl::assets::{self, AssetSpec, ResolveError};
use crate::hl::exchange::{build_for_mode, AssetRef};
use crate::hl::info::InfoClient;
use crate::hl::market::{now_ms, MarketState};
use crate::hl::ws::{FeedConfig, MarketFeed};
use crate::mode::{Authorized, Credentials, MainnetConfirmed};
use crate::monitor::alerts::{self, DailySummary, DayTracker};
use crate::monitor::commands::{CommandsHandle, StatusSnapshot};
use crate::monitor::csv::{Journal, SignalRow};
use crate::monitor::telegram::{AlertSettings, Alerter};
use crate::monitor::Monitor;
use crate::risk::kill::risk_state_path;
use crate::risk::{RiskManager, RiskParams, RiskState};
use crate::runlock::{RunLock, LOCK_REFRESH_MS};
use crate::strategy::premium::Side;
use crate::strategy::signal::{
    Command, DoneReason, Event, OrderUpdate, Phase, Strategy, StrategyParams,
};
use crate::strategy::sizing::lot_epsilon;

/// Upper bound of command -> feedback -> command rounds per input.
const MAX_PUMP_ROUNDS: usize = 32;
/// Position mismatches tolerated (consecutive reconciliations) before halting.
const MISMATCH_TOLERANCE: u32 = 2;
/// How long shutdown waits for queued Telegram alerts.
const ALERT_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long shutdown waits for the Telegram command task.
const COMMANDS_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Everything the loop needs besides the executor.
struct Ctx {
    cfg: Config,
    mode: Mode,
    coin: String,
    data_dir: PathBuf,
    state_file: PathBuf,
    risk_file: PathBuf,
    sz_decimals: u32,
    asset_id: u32,
    /// "Running" marker in the data dir (removed on drop).
    lock: RunLock,
    /// CSV journals + alerts.
    mon: Monitor,
    /// The last state save failed (alert once per failure streak).
    save_failed: bool,
    /// Daily-summary tracker (persisted with the bot state).
    day: DayTracker,
    /// Persisted tracker of a day whose summary is still due.
    missed_day: Option<DayTracker>,
    /// Snapshot for the read-only Telegram commands (`None` when they are
    /// disabled: nothing is built then).
    status: Option<watch::Sender<Arc<StatusSnapshot>>>,
    /// Live modes: traded account and signer addresses (never the key), for
    /// the startup alert.
    identity: Option<String>,
}

/// Run the bot until SIGINT / SIGTERM. `auth` comes from
/// [`crate::mode::authorize`] (the mode guard already passed).
pub async fn run(cfg: Config, auth: Authorized) -> anyhow::Result<()> {
    let Authorized {
        mode,
        creds,
        mainnet,
        ..
    } = auth;
    let alerter = Alerter::from_env(
        cfg.monitor.telegram_enabled,
        |k| std::env::var(k).ok(),
        AlertSettings {
            prefix: msg_prefix(mode, &cfg.market.coin),
            max_per_min: cfg.monitor.telegram_max_per_min,
            ..AlertSettings::default()
        },
    );
    let res = run_inner(cfg, mode, creds, mainnet, alerter.clone()).await;
    if let Err(e) = &res {
        let msg = crate::mode::scrub_secrets(&format!("antbot stopped on error: {e:#}"));
        tracing::error!("{msg}");
        alerter.send(alerts::error(&msg));
    }
    alerter.flush(ALERT_FLUSH_TIMEOUT).await;
    alerter.log_stats();
    res
}

async fn run_inner(
    cfg: Config,
    mode: Mode,
    creds: Option<Credentials>,
    mainnet: Option<MainnetConfirmed>,
    alerter: Alerter,
) -> anyhow::Result<()> {
    let ep = cfg.endpoints(mode);
    let info = InfoClient::from_config(&cfg, &ep.info_url)?;
    let data_dir = cfg.monitor.data_dir.clone();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let lock = RunLock::acquire(&data_dir, &mode.to_string(), now_ms())?;

    let spec = assets::resolve_live(&info, &cfg.market.dex, &cfg.market.coin)
        .await
        .context("resolving the traded asset")?;
    tracing::info!(
        coin = %spec.coin,
        asset_id = spec.asset_id,
        sz_decimals = spec.sz_decimals,
        max_leverage = spec.max_leverage,
        growth_mode = ?spec.growth_mode,
        "asset resolved"
    );

    let mut ctx = Ctx {
        mode,
        coin: cfg.market.coin.clone(),
        state_file: state_path(&data_dir, mode),
        risk_file: risk_state_path(&data_dir),
        mon: Monitor::new(
            Journal::open(&data_dir, &mode.to_string()),
            alerter,
            &cfg.market.coin,
        ),
        data_dir,
        sz_decimals: spec.sz_decimals,
        asset_id: spec.asset_id,
        lock,
        save_failed: false,
        day: DayTracker::new(now_ms()),
        missed_day: None,
        status: None,
        identity: None,
        cfg,
    };

    // Persisted state.
    let risk_state = RiskState::load(&ctx.risk_file)?.unwrap_or_default();
    let saved = BotState::load(&ctx.state_file)?;
    if let Some(s) = &saved {
        if s.coin != ctx.coin {
            anyhow::bail!(
                "state file {} is for {} (config trades {}): move it away first",
                ctx.state_file.display(),
                s.coin,
                ctx.coin
            );
        }
        tracing::info!(
            file = %ctx.state_file.display(),
            phase = s.strategy.phase.kind().as_str(),
            trades = s.ledger.trades.len(),
            realized_pnl_usd = s.ledger.realized_pnl_usd,
            "resuming persisted state"
        );
    }
    let sp = StrategyParams::from_config(&ctx.cfg, spec.sz_decimals, spec.max_leverage);
    let (strategy, ledger, exec_saved, premium_samples, day_saved) = match saved {
        Some(s) => (
            Strategy::from_state(sp, s.strategy),
            s.ledger,
            Some(s.executor),
            s.premium_samples,
            s.day_tracker,
        ),
        None => (Strategy::new(sp), Ledger::default(), None, Vec::new(), None),
    };
    (ctx.day, ctx.missed_day) = DayTracker::resume(day_saved, now_ms());
    let mut engine = Engine::new(
        EngineParams::from_config(&ctx.cfg),
        strategy,
        RiskManager::new(RiskParams::from(&ctx.cfg.risk), risk_state),
        ledger,
        ctx.cfg.strategy.premium_avg_secs * 1000,
    );
    engine.restore_premium_samples(now_ms(), &premium_samples);
    if ctx.cfg.strategy.premium_avg_secs > 0 {
        tracing::info!(
            window_secs = ctx.cfg.strategy.premium_avg_secs,
            restored_samples = premium_samples.len(),
            ready = engine.premium_ready(),
            "premium rolling mean (no signal until the window is covered)"
        );
    }
    if let Some(k) = engine.risk.kill_record() {
        tracing::error!(kill = %k.message, "kill switch active (persisted): no new entries until `antbot reset-kill --confirm`");
    }

    // Baseline / spec check before anything trades.
    let now = now_ms();
    let first = engine.on_spec(now, Ok(&spec));

    match mode {
        Mode::Paper => {
            let exec = PaperExecutor::from_saved(
                PaperParams::from_config(&ctx.cfg, spec.sz_decimals),
                exec_saved,
            )
            .context("restoring paper state")?;
            let mut fc = FeedConfig::from_config(&ctx.cfg, &ep.ws_url, None);
            fc.trades = true;
            let feed = MarketFeed::spawn(fc, info.clone());
            tracing::info!(
                balance_usd = exec.sim().equity(None),
                open_orders = exec.sim().open_orders().len(),
                "paper executor ready (simulated fills, no key, /exchange never called)"
            );
            main_loop(ctx, engine, exec, feed, info, first).await
        }
        Mode::Testnet | Mode::Mainnet => {
            let creds = creds.context("live mode requires HL_PRIVATE_KEY")?;
            let client = build_for_mode(&ctx.cfg, mode, &creds, mainnet, info.bucket().clone())?;
            let expires =
                (ctx.cfg.live.expires_after_ms > 0).then_some(ctx.cfg.live.expires_after_ms);
            let client = client.with_expires_after_window(expires);
            let user = client.account_address().to_hex();
            let signer = client.signer_address().to_hex();
            ctx.identity = Some(if client.is_agent() {
                format!("account {user} (main), signer {signer} (API wallet)")
            } else {
                format!("account {user} (signer = account)")
            });
            tracing::warn!(
                mode = %mode,
                exchange = client.url(),
                signer = %signer,
                account = %user,
                "LIVE executor: real orders will be sent{}",
                if client.is_mainnet() { " (MODE: MAINNET - REAL FUNDS)" } else { "" }
            );
            let api = HlApi {
                exchange: client,
                info: info.clone(),
                asset: AssetRef::from(&spec),
                dex: ctx.cfg.market.dex.clone(),
                coin: ctx.coin.clone(),
                user: user.clone(),
            };
            let mut exec = LiveExecutor::new(
                LiveParams::from_config(&ctx.cfg, spec.sz_decimals, spec.max_leverage),
                api,
                exec_saved,
                now,
            )?;
            let feed = MarketFeed::spawn(
                FeedConfig::from_config(&ctx.cfg, &ep.ws_url, Some(user)),
                info.clone(),
            );
            let evs = exec
                .startup(now_ms())
                .await
                .context("startup reconciliation")?;
            let m = feed.snapshot();
            let mut cmds = first;
            cmds.extend(apply_events(&mut engine, &mut ctx.mon, now_ms(), evs));
            let n = now_ms();
            cmds.extend(reconcile_position_at_startup(
                &ctx,
                &mut engine,
                &exec,
                n,
                &m,
            ));
            main_loop(ctx, engine, exec, feed, info, cmds).await
        }
    }
}

/// Compare the strategy's position with the venue after the startup
/// reconciliation: adopt an unknown position (config) or halt.
fn reconcile_position_at_startup<A: crate::exec::live::LiveApi>(
    ctx: &Ctx,
    engine: &mut Engine,
    exec: &LiveExecutor<A>,
    now: u64,
    m: &MarketState,
) -> Vec<Command> {
    let Some(v) = exec.venue() else {
        return Vec::new();
    };
    let eps = lot_epsilon(ctx.sz_decimals);
    let ours = engine.strategy_position();
    if (ours - v.szi).abs() <= eps {
        return Vec::new();
    }
    let flat_ok =
        ours.abs() <= eps && matches!(engine.strategy.phase(), Phase::Flat | Phase::Halted { .. });
    let px = v.entry_px.or(m.mark_px).unwrap_or(0.0);
    let ref_px = m.mark_px.unwrap_or(px).max(px);
    let cap = ctx.cfg.sizing.max_notional_usd.min(HARD_CAP_NOTIONAL_USD);
    let notional = v.szi.abs() * ref_px;
    if flat_ok && ctx.cfg.live.adopt_position && notional > cap * (1.0 + 1e-9) {
        let msg = format!(
            "venue position {} (~{notional:.0} USD) exceeds the {cap} USD cap: not adopting it — halting without trading",
            v.szi
        );
        ctx.mon.alert(&msg);
        return engine.manual_kill_with(now, &msg, false);
    }
    if flat_ok && ctx.cfg.live.adopt_position && px > 0.0 {
        let side = if v.szi > 0.0 { Side::Long } else { Side::Short };
        match engine.strategy.adopt_position(side, v.szi.abs(), px, now) {
            Ok(()) => {
                ctx.mon.alert(&format!(
                    "adopted existing {} position of {} @ {px} (managed with a fresh {} h clock)",
                    side.as_str(),
                    v.szi.abs(),
                    ctx.cfg.strategy.max_hold_secs / 3600
                ));
                return Vec::new();
            }
            Err(e) => tracing::error!(error = %e, "cannot adopt the venue position"),
        }
    }
    let msg = format!(
        "position mismatch at startup: bot {ours} vs exchange {} — halting without trading",
        v.szi
    );
    ctx.mon.alert(&msg);
    engine.manual_kill_with(now, &msg, false)
}

/// Feed executor events to the engine (order feedback is journaled);
/// returns the resulting commands.
fn apply_events(
    engine: &mut Engine,
    mon: &mut Monitor,
    now: u64,
    evs: Vec<ExecEvent>,
) -> Vec<Command> {
    let mut cmds = Vec::new();
    for ev in evs {
        if let ExecEvent::Order(u) = &ev {
            mon.order_update(now, u);
        }
        cmds.extend(engine.on_exec(now, ev));
    }
    cmds
}

/// Execute commands until no more feedback produces new ones. Commands
/// still pending after [`MAX_PUMP_ROUNDS`] are kept in `carry` and executed
/// first on the next call (never dropped: the strategy already counts them
/// as sent).
async fn pump<E: Executor>(
    engine: &mut Engine,
    exec: &mut E,
    mon: &mut Monitor,
    m: &MarketState,
    carry: &mut Vec<Command>,
    cmds: Vec<Command>,
) {
    let mut pending = std::mem::take(carry);
    pending.extend(cmds);
    for _ in 0..MAX_PUMP_ROUNDS {
        if pending.is_empty() {
            return;
        }
        let now = now_ms();
        for c in &pending {
            mon.command(now, c, (m.best_bid.map(|l| l.px), m.best_ask.map(|l| l.px)));
        }
        let evs = exec.execute(now, std::mem::take(&mut pending), m).await;
        pending = apply_events(engine, mon, now, evs);
    }
    tracing::warn!(
        pending = pending.len(),
        "command loop did not settle; the rest runs next tick"
    );
    *carry = pending;
}

/// Self-healing of the strategy's working order against the executor: a
/// `Done` that never arrived (command lost at shutdown or in a crash, event
/// dropped) is synthesised from what the executor knows, and a cancel the
/// executor never received is sent again. Without this the strategy would
/// wait forever and could neither exit nor flatten on a kill.
pub fn heal_working<E: Executor>(
    engine: &mut Engine,
    exec: &E,
    mon: &mut Monitor,
    now: u64,
) -> Vec<Command> {
    let Some(w) = engine.strategy.working() else {
        return Vec::new();
    };
    let (tag, cancel_requested) = (w.intent.tag, w.cancel_requested);
    let (reason, filled_sz) = match exec.order_view(tag) {
        OrderView::Active {
            cancel_requested: false,
        } if cancel_requested => {
            tracing::warn!(tag, "re-sending a cancel the executor never received");
            return vec![Command::Cancel { tag }];
        }
        OrderView::Active { .. } => return Vec::new(),
        OrderView::Done { reason, filled_sz } => (reason, filled_sz),
        OrderView::Unknown => (
            DoneReason::Rejected("order unknown to the executor (never sent)".into()),
            Some(0.0),
        ),
    };
    mon.alert(&format!(
        "strategy order {tag} was waiting for a terminal state the executor already settled ({reason:?}): resynchronised"
    ));
    let done = OrderUpdate::Done {
        tag,
        reason,
        filled_sz,
    };
    mon.order_update(now, &done);
    engine.on_exec(now, ExecEvent::Order(done))
}

/// Round for log output only (VWAP prices and notionals carry float noise
/// such as `101.49999999999999`).
fn round_to(v: f64, decimals: i32) -> f64 {
    let k = 10f64.powi(decimals);
    let r = (v * k).round() / k;
    if r.is_finite() {
        r
    } else {
        v
    }
}

/// Log the engine's notices, journal closed trades and send the alerts.
/// Returns whether the state should be saved now.
fn log_notices(engine: &mut Engine, mon: &mut Monitor) -> bool {
    let mut persist = false;
    for n in engine.drain_notices() {
        match n {
            Notice::Strategy(_, ev) => match ev {
                Event::EntryStarted {
                    side,
                    premium_bps,
                    px,
                    size,
                    notional_usd,
                } => {
                    tracing::info!(
                        side = side.as_str(),
                        premium_bps = round_to(premium_bps, 2),
                        px = round_to(px, 8),
                        size = round_to(size, 8),
                        notional_usd = round_to(notional_usd, 2),
                        "entry started (ALO)"
                    );
                }
                Event::EntryAborted { reason } => {
                    tracing::info!(?reason, "entry aborted");
                    persist = true;
                }
                Event::PositionOpened {
                    side, size, avg_px, ..
                } => {
                    let premium = engine.strategy.position().and_then(|p| p.entry_premium_bps);
                    tracing::info!(
                        side = side.as_str(),
                        size = round_to(size, 8),
                        avg_px = round_to(avg_px, 8),
                        entry_premium_bps = ?premium.map(|p| round_to(p, 2)),
                        "position opened"
                    );
                    mon.notify(alerts::position_opened(
                        &mon.coin, side, size, avg_px, premium,
                    ));
                    persist = true;
                }
                Event::Requote {
                    tag,
                    old_px,
                    new_px,
                } => {
                    tracing::info!(tag, old_px, new_px, "requote");
                }
                Event::ExitStarted {
                    reason,
                    premium_bps,
                    taker_at_ms,
                } => {
                    tracing::info!(
                        reason = reason.as_str(),
                        ?premium_bps,
                        taker_at_ms,
                        "exit started"
                    );
                    mon.notify(alerts::exit_started(&mon.coin, reason, premium_bps));
                }
                Event::TakerFallback { remaining } => {
                    tracing::warn!(remaining, "taker fallback (IOC)");
                    mon.notify(alerts::taker_fallback(&mon.coin, remaining));
                }
                Event::OrderRejected { tag, reason } => {
                    tracing::info!(tag, ?reason, "order rejected");
                }
                Event::TradeClosed(t) => {
                    mon.trade(&t);
                    mon.notify(alerts::trade_closed(&mon.coin, &t));
                    tracing::info!(
                        pnl_usd = t.pnl_usd,
                        pnl_bps = t.pnl_bps,
                        "{}",
                        format_args!(
                        "trade closed: {} {} {:.4} -> {:.4}, pnl {:.2} USD ({:.1} bps), fees {:.4}, funding {:.4}, reason {}, premium {:?} -> {:?} bps",
                        t.side.as_str(),
                        t.size,
                        t.entry_px,
                        t.exit_px,
                        t.pnl_usd,
                        t.pnl_bps,
                        t.fees_usd,
                        t.funding_usd,
                        t.exit_reason.as_str(),
                        t.entry_premium_bps.map(|p| (p * 10.0).round() / 10.0),
                        t.exit_premium_bps.map(|p| (p * 10.0).round() / 10.0),
                    ));
                    persist = true;
                }
                Event::Halted { reason, flatten } => {
                    tracing::error!(%reason, flatten, "strategy halted");
                    persist = true;
                }
                Event::Resumed => tracing::info!("strategy resumed"),
                // Position disagreements (unknown fill, oversized close,
                // hard-cap refusal, fills never received...) need an
                // operator, in paper mode too.
                Event::Warning(w) => mon.alert_throttled(now_ms(), &w),
            },
            Notice::Kill(_, k) => {
                tracing::error!(
                    alert = true,
                    flatten = k.flatten,
                    "KILL SWITCH: {}",
                    k.alert
                );
                mon.notify(alerts::kill(&k.alert, k.flatten));
                persist = true;
            }
            Notice::SpecChange {
                changes, killed, ..
            } => {
                tracing::warn!(
                    alert = true,
                    killed,
                    "market spec change: {}",
                    changes.join("; ")
                );
                mon.notify(alerts::spec_change(&changes, killed));
                persist = true;
            }
            Notice::Exec(_, msg) => mon.alert(&msg),
            Notice::Funding(t, usd) => tracing::info!(time_ms = t, usd, "funding settled"),
        }
    }
    persist
}

fn save_all<E: Executor>(ctx: &mut Ctx, engine: &Engine, exec: &E) {
    let st = BotState {
        version: STATE_VERSION,
        mode: ctx.mode,
        coin: ctx.coin.clone(),
        saved_ms: now_ms(),
        strategy: engine.strategy.state().clone(),
        ledger: engine.ledger.clone(),
        executor: exec.save_state(),
        premium_samples: engine.premium_samples(),
        day_tracker: Some(ctx.day.clone()),
    };
    let res = st
        .save(&ctx.state_file)
        .context("saving bot state")
        .and_then(|_| {
            engine
                .risk
                .state()
                .save(&ctx.risk_file)
                .context("saving risk state")
        });
    match res {
        Ok(()) => {
            if std::mem::take(&mut ctx.save_failed) {
                tracing::info!("state saving works again");
            }
        }
        Err(e) if !ctx.save_failed => {
            ctx.save_failed = true;
            ctx.mon.error(&format!("{e:#}"));
        }
        Err(e) => tracing::error!(error = %format!("{e:#}"), "state saving still failing"),
    }
}

/// Meta poller: perpDexs + meta{dex} every `meta_poll_secs`. Transport
/// failures are only logged; a missing dex / coin is reported as
/// unavailable (spec-change kill rule).
fn spawn_meta_poller(
    info: InfoClient,
    dex: String,
    coin: String,
    every: Duration,
) -> mpsc::Receiver<Result<AssetSpec, String>> {
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        let mut t = tokio::time::interval(every);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t.tick().await; // the startup resolution already covered t = 0
        loop {
            t.tick().await;
            let msg = match assets::resolve_live(&info, &dex, &coin).await {
                Ok(s) => Ok(s),
                Err(e) => match e.downcast_ref::<ResolveError>() {
                    Some(r) => Err(r.to_string()),
                    None => {
                        tracing::warn!(error = %format!("{e:#}"), "meta poll failed (transport), will retry");
                        continue;
                    }
                },
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    rx
}

async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = term.recv() => "SIGTERM",
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

/// Summary of a finished UTC day (trades of that day, current account).
fn send_daily_summary<E: Executor>(ctx: &Ctx, engine: &Engine, exec: &E, done: &DayTracker) {
    let Some(d) = done.day else {
        return;
    };
    let mut s = DailySummary::from_trades(d, &engine.ledger.trades);
    s.realized_total_usd = engine.ledger.realized_pnl_usd;
    s.equity_usd = engine.equity();
    s.phase = engine.strategy.phase().kind().as_str().to_string();
    s.position = exec.venue_position().unwrap_or(engine.strategy_position());
    s.kill = engine.risk.kill_record().map(|k| k.message.clone());
    s.premium_min_bps = done.min_bps;
    s.premium_max_bps = done.max_bps;
    s.premium_last_bps = done.last_bps;
    tracing::info!(
        day = %crate::monitor::fmt_utc_day(d),
        trades = s.trades,
        wins = s.wins,
        pnl_usd = s.pnl_usd,
        realized_total_usd = s.realized_total_usd,
        "daily summary"
    );
    ctx.mon.notify(s.text(&ctx.mode.to_string(), &ctx.coin));
}

/// Header of alerts and command replies (`MAINNET` in capitals: real funds).
fn msg_prefix(mode: Mode, coin: &str) -> String {
    match mode {
        Mode::Mainnet => format!("[antbot MAINNET {coin}]"),
        _ => format!("[antbot {mode} {coin}]"),
    }
}

/// Publish the state snapshot for the Telegram commands (non-blocking;
/// no-op when commands are disabled).
fn publish_status<E: Executor>(ctx: &Ctx, engine: &Engine, exec: &E) {
    if let Some(tx) = &ctx.status {
        tx.send_replace(Arc::new(StatusSnapshot::from_engine(
            engine,
            &msg_prefix(ctx.mode, &ctx.coin),
            now_ms(),
            exec.venue_position(),
        )));
    }
}

fn every(secs: u64) -> tokio::time::Interval {
    let mut t = tokio::time::interval(Duration::from_secs(secs.max(1)));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    t
}

async fn main_loop<E: Executor>(
    mut ctx: Ctx,
    mut engine: Engine,
    mut exec: E,
    mut feed: MarketFeed,
    info: InfoClient,
    initial_cmds: Vec<Command>,
) -> anyhow::Result<()> {
    let t = &ctx.cfg.timing;
    let mut meta_rx = spawn_meta_poller(
        info,
        ctx.cfg.market.dex.clone(),
        ctx.coin.clone(),
        Duration::from_secs(t.meta_poll_secs),
    );
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut persist = every(t.state_persist_secs);
    let mut heartbeat = every(t.heartbeat_secs);
    let mut sample = every(t.signal_sample_secs);
    let mut reconcile = every(ctx.cfg.live.reconcile_secs);
    let is_live = exec.kind() == "live";
    let eps = lot_epsilon(ctx.sz_decimals);
    let mut force_resync = false;
    let mut mismatches = 0u32;
    let mut dropped_seen = feed.snapshot().user_events_dropped;
    let mut carry: Vec<Command> = Vec::new();
    let mut lock_refreshed_ms = now_ms();
    let stop = shutdown_signal();
    tokio::pin!(stop);

    // Startup: kill / resume consistency, then whatever the spec check and
    // the startup reconciliation produced.
    let m0 = feed.snapshot();
    let mut cmds = initial_cmds;
    cmds.extend(engine.startup(now_ms()));
    pump(&mut engine, &mut exec, &mut ctx.mon, &m0, &mut carry, cmds).await;
    // A working order persisted by the previous run that the executor does
    // not know (or already settled) must not block the strategy.
    let cmds = heal_working(&mut engine, &exec, &mut ctx.mon, now_ms());
    pump(&mut engine, &mut exec, &mut ctx.mon, &m0, &mut carry, cmds).await;
    log_notices(&mut engine, &mut ctx.mon);
    save_all(&mut ctx, &engine, &exec);
    tracing::info!(
        mode = %ctx.mode,
        executor = exec.kind(),
        data_dir = %ctx.data_dir.display(),
        phase = engine.strategy.phase().kind().as_str(),
        "main loop started"
    );
    ctx.mon.notify(alerts::startup(
        &ctx.mode.to_string(),
        &ctx.coin,
        ctx.asset_id,
        ctx.cfg.effective_notional_usd(),
        engine.strategy.phase().kind().as_str(),
        engine.risk.kill_record().map(|k| k.message.as_str()),
        ctx.identity.as_deref(),
    ));
    // A day whose summary was due while the bot was down.
    if let Some(missed) = ctx.missed_day.take() {
        if ctx.cfg.monitor.daily_summary {
            send_daily_summary(&ctx, &engine, &exec, &missed);
        }
        save_all(&mut ctx, &engine, &exec);
    }
    // Read-only Telegram commands: a separate task that only reads the
    // snapshot published below (stopped on drop if this function errors).
    let commands = {
        let (tx, rx) = watch::channel(Arc::new(StatusSnapshot::default()));
        let h = CommandsHandle::from_env(
            ctx.cfg.monitor.telegram_commands_enabled,
            ctx.cfg.monitor.telegram_enabled,
            |k| std::env::var(k).ok(),
            rx,
        );
        if h.is_some() {
            ctx.status = Some(tx);
            publish_status(&ctx, &engine, &exec);
        }
        h
    };

    let reason = loop {
        tokio::select! {
            sig = &mut stop => break sig,
            r = feed.state.changed() => {
                if r.is_err() {
                    ctx.mon.error("market feed stopped unexpectedly");
                    break "market feed stopped";
                }
                let m = feed.state.borrow_and_update().clone();
                let now = now_ms();
                if m.user_events_dropped > dropped_seen {
                    dropped_seen = m.user_events_dropped;
                    force_resync = true;
                    ctx.mon.alert("user events were dropped by the feed: forcing a fills / orders resync");
                }
                let mut cmds = apply_events(&mut engine, &mut ctx.mon, now, exec.on_market(now, &m));
                cmds.extend(engine.on_market(now, &m, exec.available_margin_usd()));
                pump(&mut engine, &mut exec, &mut ctx.mon, &m, &mut carry, cmds).await;
            }
            Some(trades) = feed.trades.recv() => {
                let now = now_ms();
                let m = feed.snapshot();
                let cmds = apply_events(&mut engine, &mut ctx.mon, now, exec.on_trades(now, &trades));
                pump(&mut engine, &mut exec, &mut ctx.mon, &m, &mut carry, cmds).await;
            }
            Some(ev) = feed.user_events.recv() => {
                let now = now_ms();
                let m = feed.snapshot();
                let evs = exec.on_user_event(now, ev).await;
                let cmds = apply_events(&mut engine, &mut ctx.mon, now, evs);
                pump(&mut engine, &mut exec, &mut ctx.mon, &m, &mut carry, cmds).await;
            }
            _ = tick.tick() => {
                // Time-based rules (timeouts, requotes, deadlines, staleness).
                let now = now_ms();
                let m = feed.snapshot();
                let cmds = engine.on_market(now, &m, exec.available_margin_usd());
                pump(&mut engine, &mut exec, &mut ctx.mon, &m, &mut carry, cmds).await;
                if carry.is_empty() {
                    let cmds = heal_working(&mut engine, &exec, &mut ctx.mon, now_ms());
                    pump(&mut engine, &mut exec, &mut ctx.mon, &m, &mut carry, cmds).await;
                }
                if let Some(done) = ctx.day.roll(now) {
                    if ctx.cfg.monitor.daily_summary {
                        send_daily_summary(&ctx, &engine, &exec, &done);
                    }
                    // Persist the new day now: a restart must not send the
                    // same summary again.
                    save_all(&mut ctx, &engine, &exec);
                }
                publish_status(&ctx, &engine, &exec);
                if now.saturating_sub(lock_refreshed_ms) >= LOCK_REFRESH_MS {
                    lock_refreshed_ms = now;
                    if let Err(e) = ctx.lock.refresh(now) {
                        tracing::warn!(error = %format!("{e:#}"), "refreshing the run lock failed");
                    }
                }
            }
            Some(spec) = meta_rx.recv() => {
                let now = now_ms();
                let m = feed.snapshot();
                let cmds = engine.on_spec(now, spec.as_ref().map_err(|e| e.clone()));
                pump(&mut engine, &mut exec, &mut ctx.mon, &m, &mut carry, cmds).await;
            }
            _ = reconcile.tick(), if is_live => {
                let now = now_ms();
                let m = feed.snapshot();
                let evs = exec.reconcile(now, std::mem::take(&mut force_resync)).await;
                let mut cmds = apply_events(&mut engine, &mut ctx.mon, now, evs);
                // Position check (after the strategy has seen every fill).
                if let Some(v) = exec.venue_position() {
                    let ours = engine.strategy_position();
                    if (ours - v).abs() > eps && engine.strategy.working().is_none() {
                        mismatches += 1;
                        force_resync = true;
                        tracing::warn!(bot = ours, exchange = v, count = mismatches, "position mismatch");
                        if mismatches >= MISMATCH_TOLERANCE {
                            let msg = format!("position mismatch persists: bot {ours} vs exchange {v}");
                            cmds.extend(engine.manual_kill_with(now, &msg, false));
                        }
                    } else {
                        mismatches = 0;
                    }
                }
                pump(&mut engine, &mut exec, &mut ctx.mon, &m, &mut carry, cmds).await;
            }
            _ = persist.tick() => save_all(&mut ctx, &engine, &exec),
            _ = sample.tick() => {
                let m = feed.snapshot();
                let now = now_ms();
                let v = engine.last_view().copied();
                let premium = v.and_then(|v| v.premium_bps);
                ctx.day.sample(premium.or(engine.last_raw_premium_bps()));
                ctx.mon.journal.signal(&SignalRow {
                    premium_bps: premium,
                    raw_premium_bps: engine.last_raw_premium_bps(),
                    avg_ready: engine.premium_ready(),
                    stale: v.is_none_or(|v| v.stale),
                    state: engine.strategy.phase().kind().as_str().to_string(),
                    position: engine.strategy_position(),
                    killed: engine.risk.is_killed(),
                    ..SignalRow::from_market(now, &m)
                });
                tracing::info!(
                    premium_bps = ?v.and_then(|v| v.premium_bps).map(|p| (p * 100.0).round() / 100.0),
                    raw_premium_bps = ?engine.last_raw_premium_bps().map(|p| (p * 100.0).round() / 100.0),
                    avg_ready = engine.premium_ready(),
                    api_premium_bps = ?m.api_premium.map(|p| (p * 1e6).round() / 100.0),
                    oracle = ?m.oracle_px,
                    mark = ?m.mark_px,
                    bid = ?m.best_bid.map(|l| l.px),
                    ask = ?m.best_ask.map(|l| l.px),
                    stale = v.is_none_or(|v| v.stale),
                    phase = engine.strategy.phase().kind().as_str(),
                    "premium sample"
                );
            }
            _ = heartbeat.tick() => {
                let m = feed.snapshot();
                let now = now_ms();
                let r2 = |x: f64| (x * 100.0).round() / 100.0;
                let unrealized = match (engine.strategy.position(), engine.last_mark()) {
                    (Some(p), Some(mk)) => p.unrealized_pnl(mk),
                    _ => 0.0,
                };
                tracing::info!(
                    phase = engine.strategy.phase().kind().as_str(),
                    premium_bps = ?engine.last_view().and_then(|v| v.premium_bps).map(r2),
                    raw_premium_bps = ?engine.last_raw_premium_bps().map(r2),
                    position = engine.strategy_position(),
                    unrealized_usd = r2(unrealized),
                    working_order = engine.strategy.working().map(|w| w.intent.tag),
                    equity_usd = (engine.equity() * 100.0).round() / 100.0,
                    realized_usd = (engine.ledger.realized_pnl_usd * 100.0).round() / 100.0,
                    trades = engine.ledger.trades.len(),
                    venue_position = ?exec.venue_position(),
                    killed = engine.risk.is_killed(),
                    ws_connected = m.ws_connected,
                    ws_connects = m.ws_connects,
                    rest_polls = m.rest_polls,
                    data_age_ms = ?m.data_age_ms(now),
                    "heartbeat"
                );
            }
        }
        if log_notices(&mut engine, &mut ctx.mon) {
            save_all(&mut ctx, &engine, &exec);
            publish_status(&ctx, &engine, &exec);
        }
    };

    tracing::info!(%reason, "shutting down");
    // Stop the command task now, but wait for it only after the orders
    // are handled below.
    if let Some(h) = &commands {
        h.request_stop();
    }
    ctx.status = None;
    ctx.mon.notify(alerts::shutdown(
        reason,
        engine.strategy.phase().kind().as_str(),
        engine.strategy_position(),
        engine.ledger.realized_pnl_usd,
        engine.ledger.trades.len(),
    ));
    // From here on the strategy only records feedback: an order it decided
    // to send now could not be executed, and persisting it as "working"
    // would leave the next run waiting for it.
    engine.suspend();
    if !carry.is_empty() {
        tracing::warn!(dropped = carry.len(), "commands not executed at shutdown");
    }
    let m = feed.snapshot();
    let evs = exec.shutdown(now_ms(), &m).await;
    let leftover = apply_events(&mut engine, &mut ctx.mon, now_ms(), evs);
    debug_assert!(
        leftover.is_empty(),
        "suspended strategy issued {leftover:?}"
    );
    log_notices(&mut engine, &mut ctx.mon);
    save_all(&mut ctx, &engine, &exec);
    feed.shutdown().await;
    if let Some(h) = commands {
        h.shutdown(COMMANDS_STOP_TIMEOUT).await;
    }
    tracing::info!(
        state = %ctx.state_file.display(),
        phase = engine.strategy.phase().kind().as_str(),
        "state saved, bye"
    );
    Ok(())
}
