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
//! Paper mode never builds an exchange client: the executor is chosen from
//! the mode, and [`crate::hl::exchange::ExchangeClient::new`] refuses paper
//! mode anyway.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::mpsc;

use crate::config::{Config, Mode};
use crate::engine::{Engine, EngineParams, Notice};
use crate::exec::live::{HlApi, LiveExecutor, LiveParams};
use crate::exec::paper::{PaperExecutor, PaperParams};
use crate::exec::state::{state_path, BotState, Ledger, STATE_VERSION};
use crate::exec::{ExecEvent, Executor};
use crate::hl::assets::{self, AssetSpec, ResolveError};
use crate::hl::exchange::{build_for_mode, AssetRef};
use crate::hl::info::InfoClient;
use crate::hl::market::{now_ms, MarketState};
use crate::hl::ws::{FeedConfig, MarketFeed};
use crate::mode::{Credentials, MainnetConfirmed};
use crate::risk::kill::risk_state_path;
use crate::risk::{RiskManager, RiskParams, RiskState};
use crate::strategy::premium::Side;
use crate::strategy::signal::{Command, Event, Phase, Strategy, StrategyParams};
use crate::strategy::sizing::lot_epsilon;

/// Upper bound of command -> feedback -> command rounds per input.
const MAX_PUMP_ROUNDS: usize = 32;
/// Position mismatches tolerated (consecutive reconciliations) before halting.
const MISMATCH_TOLERANCE: u32 = 2;

/// Everything the loop needs besides the executor.
struct Ctx {
    cfg: Config,
    mode: Mode,
    coin: String,
    data_dir: PathBuf,
    state_file: PathBuf,
    risk_file: PathBuf,
    sz_decimals: u32,
}

/// Run the bot until SIGINT / SIGTERM.
pub async fn run(
    cfg: Config,
    mode: Mode,
    creds: Option<Credentials>,
    mainnet: Option<MainnetConfirmed>,
) -> anyhow::Result<()> {
    let ep = cfg.endpoints(mode);
    let info = InfoClient::from_config(&cfg, &ep.info_url)?;
    let data_dir = cfg.monitor.data_dir.clone();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;

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

    let ctx = Ctx {
        mode,
        coin: cfg.market.coin.clone(),
        state_file: state_path(&data_dir, mode),
        risk_file: risk_state_path(&data_dir),
        data_dir,
        sz_decimals: spec.sz_decimals,
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
    let (strategy, ledger, exec_saved) = match saved {
        Some(s) => (
            Strategy::from_state(sp, s.strategy),
            s.ledger,
            Some(s.executor),
        ),
        None => (Strategy::new(sp), Ledger::default(), None),
    };
    let mut engine = Engine::new(
        EngineParams::from_config(&ctx.cfg),
        strategy,
        RiskManager::new(RiskParams::from(&ctx.cfg.risk), risk_state),
        ledger,
        ctx.cfg.strategy.premium_avg_secs * 1000,
    );
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
            tracing::warn!(
                mode = %mode,
                exchange = client.url(),
                signer = %client.signer_address(),
                account = %user,
                "LIVE executor: real orders will be sent"
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
                LiveParams::from_config(&ctx.cfg, spec.sz_decimals),
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
            cmds.extend(apply_events(&mut engine, now_ms(), evs));
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
    if flat_ok && ctx.cfg.live.adopt_position {
        let side = if v.szi > 0.0 { Side::Long } else { Side::Short };
        let px = v.entry_px.or(m.mark_px).unwrap_or(0.0);
        match engine.strategy.adopt_position(side, v.szi.abs(), px, now) {
            Ok(()) => {
                alert(&format!(
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
    alert(&msg);
    engine.manual_kill_with(now, &msg, false)
}

/// Feed executor events to the engine; returns the resulting commands.
fn apply_events(engine: &mut Engine, now: u64, evs: Vec<ExecEvent>) -> Vec<Command> {
    let mut cmds = Vec::new();
    for ev in evs {
        cmds.extend(engine.on_exec(now, ev));
    }
    cmds
}

/// Execute commands until no more feedback produces new ones.
async fn pump<E: Executor>(
    engine: &mut Engine,
    exec: &mut E,
    m: &MarketState,
    mut cmds: Vec<Command>,
) {
    for _ in 0..MAX_PUMP_ROUNDS {
        if cmds.is_empty() {
            return;
        }
        let now = now_ms();
        let evs = exec.execute(now, std::mem::take(&mut cmds), m).await;
        cmds = apply_events(engine, now, evs);
    }
    tracing::warn!(
        pending = cmds.len(),
        "command loop did not settle; continuing next tick"
    );
}

/// Operator alert (Telegram is wired in the monitoring layer).
fn alert(msg: &str) {
    tracing::warn!(alert = true, "{msg}");
}

fn log_notices(engine: &mut Engine) -> bool {
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
                        premium_bps,
                        px,
                        size,
                        notional_usd,
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
                    alert(&format!(
                        "position opened: {} {size} @ {avg_px:.4}",
                        side.as_str()
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
                }
                Event::TakerFallback { remaining } => {
                    tracing::warn!(remaining, "taker fallback (IOC)");
                }
                Event::OrderRejected { tag, reason } => {
                    tracing::info!(tag, ?reason, "order rejected");
                }
                Event::TradeClosed(t) => {
                    alert(&format!(
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
                Event::Warning(w) => tracing::warn!("{w}"),
            },
            Notice::Kill(_, k) => {
                alert(&k.alert);
                persist = true;
            }
            Notice::SpecChange {
                changes, killed, ..
            } => {
                alert(&format!(
                    "market spec change{}: {}",
                    if killed { " (KILL)" } else { "" },
                    changes.join("; ")
                ));
                persist = true;
            }
            Notice::Exec(_, msg) => alert(&msg),
            Notice::Funding(t, usd) => tracing::info!(time_ms = t, usd, "funding settled"),
        }
    }
    persist
}

fn save_all<E: Executor>(ctx: &Ctx, engine: &Engine, exec: &E) {
    let st = BotState {
        version: STATE_VERSION,
        mode: ctx.mode,
        coin: ctx.coin.clone(),
        saved_ms: now_ms(),
        strategy: engine.strategy.state().clone(),
        ledger: engine.ledger.clone(),
        executor: exec.save_state(),
    };
    if let Err(e) = st.save(&ctx.state_file) {
        tracing::error!(error = %format!("{e:#}"), "saving bot state failed");
    }
    if let Err(e) = engine.risk.state().save(&ctx.risk_file) {
        tracing::error!(error = %format!("{e:#}"), "saving risk state failed");
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

fn every(secs: u64) -> tokio::time::Interval {
    let mut t = tokio::time::interval(Duration::from_secs(secs.max(1)));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    t
}

async fn main_loop<E: Executor>(
    ctx: Ctx,
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
    let stop = shutdown_signal();
    tokio::pin!(stop);

    // Startup: kill / resume consistency, then whatever the spec check and
    // the startup reconciliation produced.
    let m0 = feed.snapshot();
    let mut cmds = initial_cmds;
    cmds.extend(engine.startup(now_ms()));
    pump(&mut engine, &mut exec, &m0, cmds).await;
    log_notices(&mut engine);
    save_all(&ctx, &engine, &exec);
    tracing::info!(
        mode = %ctx.mode,
        executor = exec.kind(),
        data_dir = %ctx.data_dir.display(),
        phase = engine.strategy.phase().kind().as_str(),
        "main loop started"
    );

    let reason = loop {
        tokio::select! {
            sig = &mut stop => break sig,
            r = feed.state.changed() => {
                if r.is_err() {
                    break "market feed stopped";
                }
                let m = feed.state.borrow_and_update().clone();
                let now = now_ms();
                if m.user_events_dropped > dropped_seen {
                    dropped_seen = m.user_events_dropped;
                    force_resync = true;
                    alert("user events were dropped by the feed: forcing a fills / orders resync");
                }
                let mut cmds = apply_events(&mut engine, now, exec.on_market(now, &m));
                cmds.extend(engine.on_market(now, &m, exec.available_margin_usd()));
                pump(&mut engine, &mut exec, &m, cmds).await;
            }
            Some(trades) = feed.trades.recv() => {
                let now = now_ms();
                let m = feed.snapshot();
                let cmds = apply_events(&mut engine, now, exec.on_trades(now, &trades));
                pump(&mut engine, &mut exec, &m, cmds).await;
            }
            Some(ev) = feed.user_events.recv() => {
                let now = now_ms();
                let m = feed.snapshot();
                let evs = exec.on_user_event(now, ev).await;
                let cmds = apply_events(&mut engine, now, evs);
                pump(&mut engine, &mut exec, &m, cmds).await;
            }
            _ = tick.tick() => {
                // Time-based rules (timeouts, requotes, deadlines, staleness).
                let now = now_ms();
                let m = feed.snapshot();
                let cmds = engine.on_market(now, &m, exec.available_margin_usd());
                pump(&mut engine, &mut exec, &m, cmds).await;
            }
            Some(spec) = meta_rx.recv() => {
                let now = now_ms();
                let m = feed.snapshot();
                let cmds = engine.on_spec(now, spec.as_ref().map_err(|e| e.clone()));
                pump(&mut engine, &mut exec, &m, cmds).await;
            }
            _ = reconcile.tick(), if is_live => {
                let now = now_ms();
                let m = feed.snapshot();
                let evs = exec.reconcile(now, std::mem::take(&mut force_resync)).await;
                let mut cmds = apply_events(&mut engine, now, evs);
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
                pump(&mut engine, &mut exec, &m, cmds).await;
            }
            _ = persist.tick() => save_all(&ctx, &engine, &exec),
            _ = sample.tick() => {
                let m = feed.snapshot();
                let v = engine.last_view().copied();
                tracing::info!(
                    premium_bps = ?v.and_then(|v| v.premium_bps).map(|p| (p * 100.0).round() / 100.0),
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
                tracing::info!(
                    phase = engine.strategy.phase().kind().as_str(),
                    position = engine.strategy_position(),
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
        if log_notices(&mut engine) {
            save_all(&ctx, &engine, &exec);
        }
    };

    tracing::info!(%reason, "shutting down");
    let m = feed.snapshot();
    let evs = exec.shutdown(now_ms(), &m).await;
    let _ = apply_events(&mut engine, now_ms(), evs); // no new orders now
    log_notices(&mut engine);
    save_all(&ctx, &engine, &exec);
    feed.shutdown().await;
    tracing::info!(
        state = %ctx.state_file.display(),
        phase = engine.strategy.phase().kind().as_str(),
        "state saved, bye"
    );
    Ok(())
}
