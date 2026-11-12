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
