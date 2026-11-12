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
    /// Replay the strategy over historical data.
    Replay {
        /// Optional input data (directory or file) for the replay.
        #[arg(long)]
        input: Option<PathBuf>,
    },
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
        Command::Replay { input } => {
            anyhow::bail!(
                "replay is not implemented yet (input: {:?}) — TODO(step5)",
                input
            )
        }
    }
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
            (Some(c), true) => format!(
                "loaded (account: {})",
                c.account_address.as_deref().unwrap_or("derived from key")
            ),
            (None, true) => "MISSING".to_string(),
        }
    );
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
    let (m, _creds) = resolve(&cfg, args)?;
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
    anyhow::bail!("run loop not implemented yet — TODO(step5)")
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
