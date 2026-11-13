use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

use antbot::config::{Config, Mode, DEFAULT_CONFIG_PATH};
use antbot::mode::{self, load_credentials, process_env, requires_key, resolve_mode};
use antbot::monitor::logging;

#[derive(Debug, Parser)]
#[command(
    name = "antbot",
    version,
    about = "xyz:ANT HIP-3 premium mean-reversion bot (Hyperliquid)"
)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long, short, global = true, default_value = DEFAULT_CONFIG_PATH, env = "ANTBOT_CONFIG")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct ModeArgs {
    /// Override the config mode: paper | testnet | mainnet.
    #[arg(long)]
    mode: Option<Mode>,
    /// Required (with ANTBOT_CONFIRM_MAINNET=yes-i-understand) for mainnet.
    #[arg(long)]
    mainnet: bool,
}

#[derive(Debug, Args)]
struct ReplayArgs {
    /// Directory with candles_1h.json and funding_history.json (or
    /// api_funding_history.json), looked up in the directory and its parent.
    #[arg(long, conflicts_with = "fetch")]
    input: Option<PathBuf>,
    /// Explicit candles file (with --funding).
    #[arg(long, requires = "funding", conflicts_with_all = ["input", "fetch"])]
    candles: Option<PathBuf>,
    /// Explicit funding history file (with --candles).
    #[arg(long, requires = "candles")]
    funding: Option<PathBuf>,
    /// Fetch candleSnapshot + fundingHistory from the mainnet info API
    /// (read-only).
    #[arg(long)]
    fetch: bool,
    /// Days of history to fetch.
    #[arg(long, default_value_t = 30)]
    days: u64,
    /// Save the fetched data here (candles_1h.json, funding_history.json).
    #[arg(long, requires = "fetch")]
    save_dir: Option<PathBuf>,
    /// Research results.json to compare with (S2_ANT base rule / fee case).
    #[arg(long)]
    compare: Option<PathBuf>,
    /// Disable the early exit on convergence (pure max-hold, like the
    /// research 8h_fixed rule).
    #[arg(long)]
    no_early_exit: bool,
    /// Synthetic book spread in bps.
    #[arg(long, default_value_t = 2.0)]
    spread_bps: f64,
    /// Engine tick inside each hour, seconds.
    #[arg(long, default_value_t = 60)]
    step_secs: u64,
    /// Write the replayed trades as CSV.
    #[arg(long)]
    trades_csv: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the bot (paper by default).
    Run {
        #[command(flatten)]
        mode: ModeArgs,
        /// Override monitor.data_dir.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Replay the strategy + paper fill model over historical hourly data
    /// (fundingHistory premium + 1h candles). Read-only, never trades.
    Replay(ReplayArgs),
    /// Read-only market probe: resolve the asset (perpDexs + meta) and stream
    /// book / oracle / premium for a while. Never needs a key, never trades.
    Probe {
        /// Duration in seconds.
        #[arg(long, default_value_t = 20)]
        secs: u64,
        /// Print interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Use the testnet info/ws endpoints instead of mainnet.
        #[arg(long)]
        testnet: bool,
        /// Disable the websocket and exercise the REST fallback only.
        #[arg(long)]
        rest_only: bool,
    },
    /// Load + validate the config and the mode guard, then print a summary.
    CheckConfig {
        #[command(flatten)]
        mode: ModeArgs,
    },
    /// Show the persisted kill-switch state (<data_dir>/risk_state.json).
    KillStatus {
        /// Override monitor.data_dir.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Clear a persisted kill so the bot may trade again (manual reset).
    /// Also clears the hit-rate window, drawdown peak, daily start and spec
    /// baseline so the same rule does not fire again at once. Stop the bot first.
    ResetKill {
        /// Override monitor.data_dir.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Required: confirms the operator reviewed the kill reason.
        #[arg(long)]
        confirm: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Optional .env (never committed). Paper mode never reads the key from it.
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    match real_main(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main(cli: Cli) -> anyhow::Result<()> {
    let cfg = Config::load(&cli.config)?;
    match cli.command {
        Command::CheckConfig { mode } => check_config(&cfg, &cli.config, &mode),
        Command::Run { mode, data_dir } => run(cfg, &mode, data_dir).await,
        Command::Probe {
            secs,
            interval_ms,
            testnet,
            rest_only,
        } => probe(&cfg, secs, interval_ms, testnet, rest_only).await,
        Command::KillStatus { data_dir } => kill_status(&cfg, data_dir),
        Command::ResetKill { data_dir, confirm } => reset_kill(&cfg, data_dir, confirm),
        Command::Replay(args) => replay(cfg, args).await,
    }
}

fn risk_path(cfg: &Config, data_dir: Option<PathBuf>) -> PathBuf {
    let dir = data_dir.unwrap_or_else(|| cfg.monitor.data_dir.clone());
    antbot::risk::kill::risk_state_path(&dir)
}

fn kill_status(cfg: &Config, data_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let path = risk_path(cfg, data_dir);
    let Some(st) = antbot::risk::RiskState::load(&path)? else {
        println!(
            "no risk state at {} (never run): not killed",
            path.display()
        );
        return Ok(());
    };
    match &st.kill {
        None => println!("not killed ({})", path.display()),
        Some(k) => {
            println!("KILLED at {} ms: {}", k.at_ms, k.message);
            for (t, r) in &k.later {
                println!("  later trigger at {t} ms: {}", r.describe());
            }
        }
    }
    let hits = st.outcomes.iter().filter(|h| **h).count();
    println!(
        "hit window: {hits}/{} | trades closed: {} | equity: {:?} (peak {:?}, day start {:?})",
        st.outcomes.len(),
        st.trades_closed,
        st.last_equity_usd,
        st.peak_equity_usd,
        st.day_start_equity_usd
    );
    Ok(())
}

fn reset_kill(cfg: &Config, data_dir: Option<PathBuf>, confirm: bool) -> anyhow::Result<()> {
    let path = risk_path(cfg, data_dir);
    let Some(mut st) = antbot::risk::RiskState::load(&path)? else {
        println!("no risk state at {}: nothing to reset", path.display());
        return Ok(());
    };
    let Some(k) = st.kill.as_ref() else {
        println!("not killed: nothing to reset");
        return Ok(());
    };
    if !confirm {
        anyhow::bail!(
            "kill active ({}). Re-run with --confirm to clear it",
            k.message
        );
    }
    // A running bot keeps its kill in memory and rewrites the risk state:
    // a reset now would be silently undone.
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    if let Some(l) = antbot::runlock::held(dir, antbot::hl::market::now_ms()) {
        anyhow::bail!(
            "antbot is running on this data dir (pid {}, mode {}): stop it before reset-kill",
            l.pid,
            l.mode
        );
    }
    let cleared = antbot::risk::kill::reset_state(&mut st);
    st.save(&path)?;
    if let Some(k) = cleared {
        println!("kill cleared: {}", k.message);
    }
    Ok(())
}

fn resolve(cfg: &Config, args: &ModeArgs) -> anyhow::Result<(Mode, Option<mode::Credentials>)> {
    let m = resolve_mode(cfg.mode, args.mode, args.mainnet, process_env)?;
    let creds = load_credentials(m, process_env)?;
    Ok((m, creds))
}

fn check_config(cfg: &Config, path: &std::path::Path, args: &ModeArgs) -> anyhow::Result<()> {
    let (m, creds) = resolve(cfg, args)?;
    let ep = cfg.endpoints(m);
    println!("config:        {} (valid)", path.display());
    println!("mode:          {m}");
    println!(
        "market:        {} (dex {})",
        cfg.market.coin, cfg.market.dex
    );
    println!("info url:      {}", ep.info_url);
    println!("ws url:        {}", ep.ws_url);
    println!(
        "exchange url:  {}",
        ep.exchange_url
            .as_deref()
            .unwrap_or("<none: paper mode never calls /exchange>")
    );
    println!(
        "key:           {}",
        match (&creds, requires_key(m)) {
            (_, false) => "not required".to_string(),
            (Some(c), true) => match antbot::hl::signing::Wallet::from_hex(&c.private_key) {
                Ok(w) => format!(
                    "loaded (signer {}, account {})",
                    w.address(),
                    c.account_address
                        .as_deref()
                        .unwrap_or("= signer (no HL_ACCOUNT_ADDRESS)")
                ),
                Err(e) => format!("INVALID ({e})"),
            },
            (None, true) => "MISSING".to_string(),
        }
    );
    if m != Mode::Paper {
        // Guard check only: no request is made.
        let token = mode::confirm_mainnet(args.mainnet, process_env).ok();
        println!(
            "exchange guard: {}",
            match antbot::hl::exchange::ExchangeClient::check_guard(m, &ep, token) {
                Ok((url, mainnet)) => format!("ok ({url}, signing for mainnet: {mainnet})"),
                Err(e) => format!("REFUSED: {e}"),
            }
        );
    }
    let s = &cfg.strategy;
    println!(
        "entry:         |premium| >= {} bps ({:?}), exit <= {} bps (early exit {}), max hold {}s",
        s.entry_threshold_bps,
        s.premium_source,
        s.exit_converge_bps,
        s.early_exit_on_converge,
        s.max_hold_secs
    );
    println!(
        "sizing:        {} USD (cap {}, hard cap {}), {}x {:?}",
        cfg.effective_notional_usd(),
        cfg.sizing.max_notional_usd,
        antbot::config::HARD_CAP_NOTIONAL_USD,
        cfg.sizing.leverage,
        cfg.sizing.margin_mode
    );
    let r = &cfg.risk;
    println!(
        "kill:          hit rate < {} over {} trades, |premium| > {} bps for {}s, daily loss {} USD, max DD {} USD, spec change {}",
        r.hit_rate_min, r.hit_rate_window, r.persistent_premium_bps, r.persistent_premium_secs,
        r.daily_loss_limit_usd, r.max_drawdown_usd, r.kill_on_spec_change
    );
    println!("data dir:      {}", cfg.monitor.data_dir.display());
    Ok(())
}

async fn run(mut cfg: Config, args: &ModeArgs, data_dir: Option<PathBuf>) -> anyhow::Result<()> {
    if let Some(d) = data_dir {
        cfg.monitor.data_dir = d;
    }
    let (m, creds) = resolve(&cfg, args)?;
    let _guard = logging::init(&cfg.monitor.data_dir, cfg.monitor.log_json)
        .context("initialising logging")?;
    let ep = cfg.endpoints(m);
    tracing::info!(
        mode = %m,
        coin = %cfg.market.coin,
        info_url = %ep.info_url,
        exchange = ep.exchange_url.is_some(),
        notional_usd = cfg.effective_notional_usd(),
        "antbot starting"
    );
    if m == Mode::Mainnet {
        // Development build: mainnet trading is intentionally not wired.
        anyhow::bail!("mainnet trading is not available in this build");
    }
    let token = if m == Mode::Mainnet {
        Some(mode::confirm_mainnet(args.mainnet, process_env)?)
    } else {
        None
    };
    antbot::runner::run(cfg, m, creds, token).await
}

async fn replay(mut cfg: Config, a: ReplayArgs) -> anyhow::Result<()> {
    use antbot::replay::{self, ReplayData, ReplayParams};
    let coin = cfg.market.coin.clone();
    let data = if a.fetch {
        // Paper endpoints: mainnet info, no exchange URL.
        let ep = cfg.endpoints(Mode::Paper);
        let info = antbot::hl::info::InfoClient::from_config(&cfg, &ep.info_url)?;
        let (data, funding, candles) =
            ReplayData::fetch(&info, &coin, a.days, antbot::hl::market::now_ms()).await?;
        if let Some(dir) = &a.save_dir {
            std::fs::create_dir_all(dir)?;
            std::fs::write(
                dir.join("candles_1h.json"),
                serde_json::to_vec(&serde_json::json!({ &coin: candles }))?,
            )?;
            std::fs::write(
                dir.join("funding_history.json"),
                serde_json::to_vec(&serde_json::json!({ &coin: funding }))?,
            )?;
            println!("saved fetched data to {}", dir.display());
        }
        data
    } else if let (Some(c), Some(f)) = (&a.candles, &a.funding) {
        ReplayData::load_files(c, f, &coin)?
    } else if let Some(dir) = &a.input {
        ReplayData::load_dir(dir, &coin)?
    } else {
        anyhow::bail!("replay needs --input DIR, --candles/--funding FILES or --fetch");
    };
    if a.no_early_exit {
        cfg.strategy.early_exit_on_converge = false;
    }
    let p = ReplayParams {
        sz_decimals: cfg.market.expected_sz_decimals,
        max_leverage: cfg.market.expected_max_leverage,
        spread_bps: a.spread_bps,
        step_secs: a.step_secs,
        ..ReplayParams::default()
    };
    let res = replay::run(&cfg, &data, &p)?;
    let refr = match &a.compare {
        Some(path) => Some(replay::load_reference(path)?),
        None => None,
    };
    print!("{}", replay::report(&cfg, &data, &res, refr.as_ref()));
    if let Some(path) = &a.trades_csv {
        replay::write_trades_csv(path, &res.trades)?;
        println!("trades written to {}", path.display());
    }
    Ok(())
}

/// Read-only market probe (info + ws only).
async fn probe(
    cfg: &Config,
    secs: u64,
    interval_ms: u64,
    testnet: bool,
    rest_only: bool,
) -> anyhow::Result<()> {
    use antbot::config::PremiumSource;
    use antbot::hl::{assets, info::InfoClient, market::now_ms, ws};
    use std::time::Duration;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,antbot=info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    // Paper endpoints = mainnet info/ws with no exchange URL.
    let ep = cfg.endpoints(if testnet { Mode::Testnet } else { Mode::Paper });
    let info = InfoClient::from_config(cfg, &ep.info_url)?;
    let (dex, coin) = (&cfg.market.dex, &cfg.market.coin);

    println!("info: {}  ws: {}", ep.info_url, ep.ws_url);
    let spec = assets::resolve_live(&info, dex, coin).await?;
    println!(
        "asset: {coin} dex={dex} perp_dex_index={} index_in_meta={} asset_id={}",
        spec.dex_index, spec.index_in_meta, spec.asset_id
    );
    println!(
        "  szDecimals={} maxPxDecimals={} maxLeverage={} onlyIsolated={} marginMode={:?} marginTableId={:?}",
        spec.sz_decimals, spec.max_price_decimals, spec.max_leverage, spec.only_isolated,
        spec.margin_mode, spec.margin_table_id
    );
    println!(
        "  growthMode={:?} deployerFeeScale={:?} lastFeeScaleChange={:?} fundingMultiplier={:?} fundingInterestRate={:?} oiCap={:?} oracleUpdater={:?} delisted={}",
        spec.growth_mode, spec.deployer_fee_scale, spec.last_fee_scale_change_time,
        spec.funding_multiplier, spec.funding_interest_rate, spec.streaming_oi_cap,
        spec.oracle_updater, spec.is_delisted
    );
    let mism = spec.check_expected(&cfg.market);
    if mism.is_empty() {
        println!("  spec matches config baseline");
    } else {
        println!("  SPEC MISMATCH vs config: {}", mism.join("; "));
    }

    let mut fc = ws::FeedConfig::from_config(cfg, &ep.ws_url, None);
    fc.ws_enabled = !rest_only;
    let feed = ws::MarketFeed::spawn(fc, info);
    let stale_ms = cfg.timing.ws_stale_secs * 1000;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut tick = tokio::time::interval(Duration::from_millis(interval_ms.max(100)));
    println!(
        "{:>13} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>8} {:>8} {:>8} {:>11} {:>7} {:>8} {:>9} {:>3}",
        "time_ms", "bid", "ask", "imp_bid", "imp_ask", "oracle", "mark", "spr_bps",
        "prem_imp", "prem_mid", "prem_api", "funding", "ctx_age", "book_age", "src", "ws"
    );
    let f = |v: Option<f64>, p: usize| v.map_or("-".to_string(), |x| format!("{x:.p$}"));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tokio::signal::ctrl_c() => break,
        }
        let s = feed.snapshot();
        let now = now_ms();
        println!(
            "{:>13} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>8} {:>8} {:>8} {:>11} {:>7} {:>8} {:>9} {:>3}",
            now,
            f(s.best_bid.map(|l| l.px), 4),
            f(s.best_ask.map(|l| l.px), 4),
            f(s.impact_bid_px, 4),
            f(s.impact_ask_px, 4),
            f(s.oracle_px, 4),
            f(s.mark_px, 4),
            f(s.spread_bps(), 2),
            f(s.premium(PremiumSource::Impact).map(|p| p * 1e4), 2),
            f(s.premium(PremiumSource::BookMid).map(|p| p * 1e4), 2),
            f(s.api_premium.map(|p| p * 1e4), 2),
            f(s.funding, 10),
            s.ctx_age_ms(now).map_or("-".into(), |a| a.to_string()),
            s.book_age_ms(now).map_or("-".into(), |a| a.to_string()),
            format!("{}/{}", s.book_source, s.ctx_source),
            if s.ws_stale(now, stale_ms) { "N" } else { "Y" },
        );
    }
    let s = feed.snapshot();
    println!(
        "done: ws_connects={} rest_polls={} updates={}",
        s.ws_connects, s.rest_polls, s.seq
    );
    feed.shutdown().await;
    Ok(())
}
