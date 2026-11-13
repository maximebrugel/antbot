//! TOML configuration: every strategy / risk / execution threshold.
//!
//! `config/default.toml` is the reference file; [`Config::default`] mirrors it
//! exactly (checked by a unit test) so partially specified user files fall back
//! to the documented defaults. Unknown keys are rejected to catch typos.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Absolute notional ceiling per position, independent of the config file.
pub const HARD_CAP_NOTIONAL_USD: f64 = 10_000.0;
/// Highest leverage the bot will ever set (sizing_fr.md §7: "1x (2x maximum)").
pub const MAX_ALLOWED_LEVERAGE: u32 = 2;
/// Path of the shipped default config, relative to the repo / working dir.
pub const DEFAULT_CONFIG_PATH: &str = "config/default.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Live mainnet market data, simulated fills, no key, never calls /exchange.
    #[default]
    Paper,
    /// Real orders on the Hyperliquid testnet.
    Testnet,
    /// Real orders on mainnet (REAL FUNDS). Guarded by `mode::authorize`: CLI
    /// flag, env confirmation, API wallet key distinct from the main account.
    Mainnet,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Paper => "paper",
            Mode::Testnet => "testnet",
            Mode::Mainnet => "mainnet",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "paper" => Ok(Mode::Paper),
            "testnet" => Ok(Mode::Testnet),
            "mainnet" => Ok(Mode::Mainnet),
            other => Err(format!(
                "unknown mode '{other}' (expected paper|testnet|mainnet)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PremiumSource {
    /// 0.5 * (impact_bid + impact_ask) / oracle - 1 (HL funding premium, backtest).
    #[default]
    Impact,
    /// 0.5 * (best_bid + best_ask) / oracle - 1.
    BookMid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HitDefinition {
    /// |premium_exit| < |premium_entry| (writeup: "moved toward 0" rate).
    #[default]
    PremiumTowardZero,
    /// Net PnL of the trade > 0.
    PnlPositive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MarginMode {
    #[default]
    Isolated,
    Cross,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub market: MarketConfig,
    pub network: NetworkConfig,
    pub strategy: StrategyConfig,
    pub sizing: SizingConfig,
    pub risk: RiskConfig,
    pub fees: FeesConfig,
    pub timing: TimingConfig,
    pub monitor: MonitorConfig,
    pub paper: PaperConfig,
    pub live: LiveConfig,
}

/// What to do with open orders on the coin that the bot does not know
/// about at startup (live modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StrayOrders {
    /// Cancel them (default): the bot assumes it owns the coin.
    #[default]
    Cancel,
    /// Leave them alone (logged and alerted, never managed).
    Keep,
}

/// Paper executor (simulated fills on live mainnet data).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaperConfig {
    /// Simulated account balance (USD), used for available margin.
    pub initial_balance_usd: f64,
    /// A resting order fills when a trade prints exactly at its price
    /// ("touch"); false = only trades through the price (pessimistic queue).
    pub fill_on_touch: bool,
    /// Queue position for touch fills: the size shown at our price when we
    /// join the level is ahead of us and must trade first.
    pub queue_model: bool,
}

/// Live executor (testnet / guarded mainnet).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LiveConfig {
    /// Unknown open orders on the coin at startup: cancel | keep.
    pub stray_orders: StrayOrders,
    /// Adopt a position found at startup when the bot thinks it is flat
    /// (it is then managed with a fresh 8h clock). false = halt instead.
    pub adopt_position: bool,
    /// Cancel the bot's open orders on SIGINT / SIGTERM.
    pub cancel_on_shutdown: bool,
    /// Also close the position (reduce-only IOC) on shutdown.
    pub flatten_on_shutdown: bool,
    /// Periodic reconciliation with clearinghouseState / openOrders.
    pub reconcile_secs: u64,
    /// Client-side cap on exchange actions per rolling minute (per-address
    /// action budget). New orders beyond it are refused; cancels always go.
    pub max_exchange_actions_per_min: u32,
    /// Pause new orders this long after a rate-limit answer.
    pub rate_limit_backoff_secs: u64,
    /// `expiresAfter` window for signed actions (ms, 0 = off).
    pub expires_after_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarketConfig {
    pub dex: String,
    pub coin: String,
    pub expected_funding_multiplier: f64,
    pub expected_growth_mode: String,
    pub expected_deployer_fee_scale: f64,
    pub expected_sz_decimals: u32,
    pub expected_max_leverage: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub mainnet_info_url: String,
    pub mainnet_exchange_url: String,
    pub mainnet_ws_url: String,
    pub testnet_info_url: String,
    pub testnet_exchange_url: String,
    pub testnet_ws_url: String,
    pub http_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StrategyConfig {
    pub premium_source: PremiumSource,
    pub entry_threshold_bps: f64,
    pub exit_converge_bps: f64,
    pub early_exit_on_converge: bool,
    pub max_hold_secs: u64,
    pub cooldown_after_exit_secs: u64,
    pub entry_timeout_secs: u64,
    pub requote_interval_secs: u64,
    pub requote_drift_bps: f64,
    pub taker_fallback_window_secs: u64,
    pub exit_maker_timeout_secs: u64,
    pub taker_max_slippage_bps: f64,
    pub premium_avg_secs: u64,
    pub entry_retry_cooldown_secs: u64,
    pub min_order_interval_ms: u64,
    pub max_order_actions_per_min: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SizingConfig {
    pub notional_usd: f64,
    pub max_notional_usd: f64,
    pub leverage: u32,
    pub margin_mode: MarginMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskConfig {
    pub hit_rate_window: usize,
    pub hit_rate_min: f64,
    pub hit_definition: HitDefinition,
    pub persistent_premium_bps: f64,
    pub persistent_premium_secs: u64,
    pub kill_on_spec_change: bool,
    pub daily_loss_limit_usd: f64,
    pub max_drawdown_usd: f64,
    pub flatten_on_kill: bool,
    pub kill_flatten_maker_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeesConfig {
    pub maker_rate: f64,
    pub taker_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimingConfig {
    pub rest_fallback_poll_secs: u64,
    pub ws_stale_secs: u64,
    pub ws_ping_secs: u64,
    pub ws_reconnect_initial_ms: u64,
    pub ws_reconnect_max_ms: u64,
    pub meta_poll_secs: u64,
    pub signal_sample_secs: u64,
    pub state_persist_secs: u64,
    pub heartbeat_secs: u64,
    pub rest_bucket_capacity: f64,
    pub rest_refill_per_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    pub data_dir: PathBuf,
    pub log_json: bool,
    /// Daily log files kept in `<data_dir>/logs` (0 = keep all; 1 acts
    /// as 2, see `monitor::logging::MIN_LOG_FILES`).
    pub log_max_files: usize,
    pub telegram_enabled: bool,
    /// Telegram messages per rolling minute (queued alerts are merged).
    pub telegram_max_per_min: u32,
    /// Answer the read-only `/pnl`, `/trades`, `/help` commands from the
    /// configured private chat (needs `telegram_enabled` and the env vars).
    pub telegram_commands_enabled: bool,
    /// Send a summary of the previous UTC day after midnight UTC.
    pub daily_summary: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Paper,
            market: MarketConfig::default(),
            network: NetworkConfig::default(),
            strategy: StrategyConfig::default(),
            sizing: SizingConfig::default(),
            risk: RiskConfig::default(),
            fees: FeesConfig::default(),
            timing: TimingConfig::default(),
            monitor: MonitorConfig::default(),
            paper: PaperConfig::default(),
            live: LiveConfig::default(),
        }
    }
}

impl Default for MarketConfig {
    fn default() -> Self {
        Self {
            dex: "xyz".into(),
            coin: "xyz:ANT".into(),
            expected_funding_multiplier: 0.005,
            expected_growth_mode: "enabled".into(),
            expected_deployer_fee_scale: 1.0,
            expected_sz_decimals: 2,
            expected_max_leverage: 10,
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            mainnet_info_url: "https://api.hyperliquid.xyz/info".into(),
            mainnet_exchange_url: "https://api.hyperliquid.xyz/exchange".into(),
            mainnet_ws_url: "wss://api.hyperliquid.xyz/ws".into(),
            testnet_info_url: "https://api.hyperliquid-testnet.xyz/info".into(),
            testnet_exchange_url: "https://api.hyperliquid-testnet.xyz/exchange".into(),
            testnet_ws_url: "wss://api.hyperliquid-testnet.xyz/ws".into(),
            http_timeout_secs: 10,
        }
    }
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            premium_source: PremiumSource::Impact,
            entry_threshold_bps: 50.0,
            exit_converge_bps: 10.0,
            early_exit_on_converge: true,
            max_hold_secs: 8 * 3600,
            cooldown_after_exit_secs: 3600,
            entry_timeout_secs: 900,
            requote_interval_secs: 20,
            requote_drift_bps: 5.0,
            taker_fallback_window_secs: 900,
            exit_maker_timeout_secs: 600,
            taker_max_slippage_bps: 50.0,
            premium_avg_secs: 3600,
            entry_retry_cooldown_secs: 60,
            min_order_interval_ms: 1000,
            max_order_actions_per_min: 30,
        }
    }
}

impl Default for SizingConfig {
    fn default() -> Self {
        Self {
            notional_usd: 2500.0,
            max_notional_usd: HARD_CAP_NOTIONAL_USD,
            leverage: 1,
            margin_mode: MarginMode::Isolated,
        }
    }
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            hit_rate_window: 20,
            hit_rate_min: 0.80,
            hit_definition: HitDefinition::PremiumTowardZero,
            persistent_premium_bps: 150.0,
            persistent_premium_secs: 7200,
            kill_on_spec_change: true,
            daily_loss_limit_usd: 150.0,
            max_drawdown_usd: 300.0,
            flatten_on_kill: true,
            kill_flatten_maker_secs: 120,
        }
    }
}

impl Default for FeesConfig {
    fn default() -> Self {
        Self {
            maker_rate: 0.00003,
            taker_rate: 0.00009,
        }
    }
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            rest_fallback_poll_secs: 5,
            ws_stale_secs: 15,
            ws_ping_secs: 50,
            ws_reconnect_initial_ms: 500,
            ws_reconnect_max_ms: 30_000,
            meta_poll_secs: 60,
            signal_sample_secs: 60,
            state_persist_secs: 30,
            heartbeat_secs: 300,
            rest_bucket_capacity: 100.0,
            rest_refill_per_sec: 10.0,
        }
    }
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            log_json: false,
            log_max_files: 30,
            telegram_enabled: true,
            telegram_max_per_min: 20,
            telegram_commands_enabled: true,
            daily_summary: true,
        }
    }
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            initial_balance_usd: 10_000.0,
            fill_on_touch: true,
            queue_model: true,
        }
    }
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            stray_orders: StrayOrders::Cancel,
            adopt_position: true,
            cancel_on_shutdown: true,
            flatten_on_shutdown: false,
            reconcile_secs: 30,
            max_exchange_actions_per_min: 60,
            rate_limit_backoff_secs: 60,
            expires_after_ms: 30_000,
        }
    }
}

/// Errors from loading / validating the config. Validation collects every
/// problem at once so the operator can fix the file in one pass.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

/// Network endpoints resolved for a mode. `exchange_url` is `None` in paper
/// mode so that no code path can ever build an /exchange request from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub info_url: String,
    pub ws_url: String,
    pub exchange_url: Option<String>,
    pub is_mainnet: bool,
}

impl Config {
    /// Parse a TOML string and validate it.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(s).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<string>"),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Read, parse and validate a config file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Check every invariant. Returns all violations at once.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut e: Vec<String> = Vec::new();
        let finite_pos = |v: f64| v.is_finite() && v > 0.0;

        // market
        let m = &self.market;
        if m.dex.trim().is_empty() {
            e.push("market.dex must not be empty".into());
        }
        if m.coin.trim().is_empty() {
            e.push("market.coin must not be empty".into());
        } else if !m.dex.is_empty() && !m.coin.starts_with(&format!("{}:", m.dex)) {
            e.push(format!(
                "market.coin '{}' must be prefixed by the dex name '{}:' (HIP-3 naming)",
                m.coin, m.dex
            ));
        }
        if !(m.expected_funding_multiplier.is_finite() && m.expected_funding_multiplier >= 0.0) {
            e.push("market.expected_funding_multiplier must be >= 0".into());
        }
        if !(m.expected_deployer_fee_scale.is_finite() && m.expected_deployer_fee_scale >= 0.0) {
            e.push("market.expected_deployer_fee_scale must be >= 0".into());
        }
        if m.expected_sz_decimals > 6 {
            e.push("market.expected_sz_decimals must be <= 6".into());
        }
        if m.expected_max_leverage == 0 {
            e.push("market.expected_max_leverage must be >= 1".into());
        }

        // network
        let n = &self.network;
        for (k, v, scheme) in [
            ("mainnet_info_url", &n.mainnet_info_url, "https://"),
            ("mainnet_exchange_url", &n.mainnet_exchange_url, "https://"),
            ("mainnet_ws_url", &n.mainnet_ws_url, "wss://"),
            ("testnet_info_url", &n.testnet_info_url, "https://"),
            ("testnet_exchange_url", &n.testnet_exchange_url, "https://"),
            ("testnet_ws_url", &n.testnet_ws_url, "wss://"),
        ] {
            if !v.starts_with(scheme) {
                e.push(format!("network.{k} must start with {scheme} (got '{v}')"));
            }
        }
        if n.testnet_exchange_url == n.mainnet_exchange_url {
            e.push("network.testnet_exchange_url must differ from mainnet_exchange_url".into());
        }
        if n.http_timeout_secs == 0 {
            e.push("network.http_timeout_secs must be > 0".into());
        }

        // strategy
        let s = &self.strategy;
        if !finite_pos(s.entry_threshold_bps) {
            e.push("strategy.entry_threshold_bps must be > 0".into());
        }
        if !(s.exit_converge_bps.is_finite() && s.exit_converge_bps >= 0.0) {
            e.push("strategy.exit_converge_bps must be >= 0".into());
        } else if s.exit_converge_bps >= s.entry_threshold_bps {
            e.push(format!(
                "strategy.exit_converge_bps ({}) must be < entry_threshold_bps ({})",
                s.exit_converge_bps, s.entry_threshold_bps
            ));
        }
        if s.max_hold_secs == 0 {
            e.push("strategy.max_hold_secs must be > 0".into());
        }
        if s.taker_fallback_window_secs >= s.max_hold_secs {
            e.push(format!(
                "strategy.taker_fallback_window_secs ({}) must be < max_hold_secs ({})",
                s.taker_fallback_window_secs, s.max_hold_secs
            ));
        }
        if s.entry_timeout_secs == 0 {
            e.push("strategy.entry_timeout_secs must be > 0".into());
        }
        if s.requote_interval_secs == 0 {
            e.push("strategy.requote_interval_secs must be > 0".into());
        }
        if !(s.requote_drift_bps.is_finite() && s.requote_drift_bps >= 0.0) {
            e.push("strategy.requote_drift_bps must be >= 0".into());
        }
        if s.exit_maker_timeout_secs == 0 {
            e.push("strategy.exit_maker_timeout_secs must be > 0".into());
        }
        if !finite_pos(s.taker_max_slippage_bps) || s.taker_max_slippage_bps > 500.0 {
            e.push("strategy.taker_max_slippage_bps must be in (0, 500]".into());
        }
        if s.premium_avg_secs > 4 * 3600 {
            e.push("strategy.premium_avg_secs must be <= 14400 (4h)".into());
        }
        if s.min_order_interval_ms > 60_000 {
            e.push("strategy.min_order_interval_ms must be <= 60000".into());
        }
        if s.max_order_actions_per_min == 0 || s.max_order_actions_per_min > 600 {
            e.push("strategy.max_order_actions_per_min must be in 1..=600".into());
        }

        // sizing
        let z = &self.sizing;
        if !finite_pos(z.notional_usd) {
            e.push("sizing.notional_usd must be > 0".into());
        }
        if !finite_pos(z.max_notional_usd) {
            e.push("sizing.max_notional_usd must be > 0".into());
        } else if z.max_notional_usd > HARD_CAP_NOTIONAL_USD {
            e.push(format!(
                "sizing.max_notional_usd ({}) exceeds the hard cap of {HARD_CAP_NOTIONAL_USD} USD",
                z.max_notional_usd
            ));
        }
        if z.notional_usd.is_finite() && z.notional_usd > z.max_notional_usd {
            e.push(format!(
                "sizing.notional_usd ({}) exceeds sizing.max_notional_usd ({})",
                z.notional_usd, z.max_notional_usd
            ));
        }
        if z.notional_usd.is_finite() && z.notional_usd > HARD_CAP_NOTIONAL_USD {
            e.push(format!(
                "sizing.notional_usd ({}) exceeds the hard cap of {HARD_CAP_NOTIONAL_USD} USD",
                z.notional_usd
            ));
        }
        if z.leverage == 0 || z.leverage > MAX_ALLOWED_LEVERAGE {
            e.push(format!(
                "sizing.leverage ({}) must be in 1..={MAX_ALLOWED_LEVERAGE}",
                z.leverage
            ));
        }
        if z.leverage > self.market.expected_max_leverage {
            e.push("sizing.leverage exceeds market.expected_max_leverage".into());
        }
        if z.margin_mode != MarginMode::Isolated {
            e.push("sizing.margin_mode must be \"isolated\"".into());
        }

        // risk
        let r = &self.risk;
        if r.hit_rate_window == 0 {
            e.push("risk.hit_rate_window must be >= 1".into());
        }
        if !(r.hit_rate_min.is_finite() && r.hit_rate_min > 0.0 && r.hit_rate_min <= 1.0) {
            e.push("risk.hit_rate_min must be in (0, 1]".into());
        }
        if !finite_pos(r.persistent_premium_bps) {
            e.push("risk.persistent_premium_bps must be > 0".into());
        } else if r.persistent_premium_bps <= s.entry_threshold_bps {
            e.push(format!(
                "risk.persistent_premium_bps ({}) must be > strategy.entry_threshold_bps ({})",
                r.persistent_premium_bps, s.entry_threshold_bps
            ));
        }
        if r.persistent_premium_secs == 0 {
            e.push("risk.persistent_premium_secs must be > 0".into());
        }
        if !finite_pos(r.daily_loss_limit_usd) {
            e.push("risk.daily_loss_limit_usd must be > 0".into());
        }
        if !finite_pos(r.max_drawdown_usd) {
            e.push("risk.max_drawdown_usd must be > 0".into());
        }
        if r.kill_flatten_maker_secs >= s.max_hold_secs {
            e.push("risk.kill_flatten_maker_secs must be < strategy.max_hold_secs".into());
        }

        // fees
        let f = &self.fees;
        for (k, v) in [("maker_rate", f.maker_rate), ("taker_rate", f.taker_rate)] {
            // Negative maker (rebate) allowed; anything beyond +/-1 % is a typo.
            if !v.is_finite() || v.abs() > 0.01 {
                e.push(format!(
                    "fees.{k} ({v}) must be a fraction within [-0.01, 0.01]"
                ));
            }
        }
        if f.taker_rate.is_finite() && f.maker_rate.is_finite() && f.taker_rate < f.maker_rate {
            e.push("fees.taker_rate must be >= fees.maker_rate".into());
        }

        // timing
        let t = &self.timing;
        for (k, v) in [
            ("rest_fallback_poll_secs", t.rest_fallback_poll_secs),
            ("ws_stale_secs", t.ws_stale_secs),
            ("ws_ping_secs", t.ws_ping_secs),
            ("ws_reconnect_initial_ms", t.ws_reconnect_initial_ms),
            ("meta_poll_secs", t.meta_poll_secs),
            ("signal_sample_secs", t.signal_sample_secs),
            ("state_persist_secs", t.state_persist_secs),
            ("heartbeat_secs", t.heartbeat_secs),
        ] {
            if v == 0 {
                e.push(format!("timing.{k} must be > 0"));
            }
        }
        if t.ws_reconnect_max_ms < t.ws_reconnect_initial_ms {
            e.push("timing.ws_reconnect_max_ms must be >= ws_reconnect_initial_ms".into());
        }
        if t.ws_ping_secs >= 60 {
            // HL closes idle connections after 60s without a message.
            e.push("timing.ws_ping_secs must be < 60".into());
        }
        if !finite_pos(t.rest_bucket_capacity) {
            e.push("timing.rest_bucket_capacity must be > 0".into());
        }
        if !finite_pos(t.rest_refill_per_sec) || t.rest_refill_per_sec > 20.0 {
            // 1200 weight/min = 20/s is the HL per-IP budget.
            e.push("timing.rest_refill_per_sec must be in (0, 20]".into());
        }

        // paper / live
        if !finite_pos(self.paper.initial_balance_usd) {
            e.push("paper.initial_balance_usd must be > 0".into());
        }
        let l = &self.live;
        if l.reconcile_secs == 0 || l.reconcile_secs > 3600 {
            e.push("live.reconcile_secs must be in [1, 3600]".into());
        }
        if l.max_exchange_actions_per_min == 0 {
            e.push("live.max_exchange_actions_per_min must be >= 1".into());
        }
        if l.expires_after_ms != 0 && !(5_000..=300_000).contains(&l.expires_after_ms) {
            e.push("live.expires_after_ms must be 0 (off) or in [5000, 300000]".into());
        }

        // monitor
        if self.monitor.data_dir.as_os_str().is_empty() {
            e.push("monitor.data_dir must not be empty".into());
        }
        if !(1..=30).contains(&self.monitor.telegram_max_per_min) {
            e.push("monitor.telegram_max_per_min must be in 1..=30 (Telegram limits)".into());
        }

        if e.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(e))
        }
    }

    /// Endpoints for a resolved mode. Paper uses mainnet market data but has
    /// no exchange URL at all.
    pub fn endpoints(&self, mode: Mode) -> Endpoints {
        let n = &self.network;
        match mode {
            Mode::Paper => Endpoints {
                info_url: n.mainnet_info_url.clone(),
                ws_url: n.mainnet_ws_url.clone(),
                exchange_url: None,
                is_mainnet: true,
            },
            Mode::Testnet => Endpoints {
                info_url: n.testnet_info_url.clone(),
                ws_url: n.testnet_ws_url.clone(),
                exchange_url: Some(n.testnet_exchange_url.clone()),
                is_mainnet: false,
            },
            Mode::Mainnet => Endpoints {
                info_url: n.mainnet_info_url.clone(),
                ws_url: n.mainnet_ws_url.clone(),
                exchange_url: Some(n.mainnet_exchange_url.clone()),
                is_mainnet: true,
            },
        }
    }

    /// Notional actually used for a new position: config value clamped to the
    /// config cap and to the built-in hard cap.
    pub fn effective_notional_usd(&self) -> f64 {
        self.sizing
            .notional_usd
            .min(self.sizing.max_notional_usd)
            .clamp(0.0, HARD_CAP_NOTIONAL_USD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_file() -> String {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_CONFIG_PATH);
        std::fs::read_to_string(p).expect("config/default.toml present")
    }

    fn invalid_msgs(cfg: &Config) -> Vec<String> {
        match cfg.validate() {
            Err(ConfigError::Invalid(v)) => v,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn default_toml_parses_validates_and_matches_default_impl() {
        let cfg = Config::from_toml_str(&default_file()).expect("default.toml valid");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn default_values_match_research() {
        let c = Config::default();
        assert_eq!(c.mode, Mode::Paper);
        assert_eq!(c.market.coin, "xyz:ANT");
        assert_eq!(c.market.dex, "xyz");
        assert_eq!(c.market.expected_funding_multiplier, 0.005);
        assert_eq!(c.strategy.entry_threshold_bps, 50.0);
        assert_eq!(c.strategy.max_hold_secs, 28_800);
        assert_eq!(c.strategy.premium_source, PremiumSource::Impact);
        assert_eq!(c.sizing.notional_usd, 2500.0);
        assert_eq!(c.sizing.max_notional_usd, 10_000.0);
        assert_eq!(c.sizing.leverage, 1);
        assert_eq!(c.sizing.margin_mode, MarginMode::Isolated);
        assert_eq!(c.risk.hit_rate_min, 0.80);
        assert_eq!(c.risk.persistent_premium_bps, 150.0);
        assert!((c.fees.maker_rate - 0.00003).abs() < 1e-12);
        assert!((c.fees.taker_rate - 0.00009).abs() < 1e-12);
        c.validate().unwrap();
    }

    #[test]
    fn empty_file_falls_back_to_defaults() {
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_override_keeps_other_defaults() {
        let cfg =
            Config::from_toml_str("mode = \"testnet\"\n[strategy]\nentry_threshold_bps = 60.0\n")
                .unwrap();
        assert_eq!(cfg.mode, Mode::Testnet);
        assert_eq!(cfg.strategy.entry_threshold_bps, 60.0);
        assert_eq!(cfg.strategy.max_hold_secs, 28_800);
        assert_eq!(cfg.sizing.notional_usd, 2500.0);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::from_toml_str("[sizing]\nnotional = 5000.0\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let err = Config::from_toml_str("typo_top_level = 1\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    }

    #[test]
    fn bad_mode_string_is_rejected() {
        assert!(Config::from_toml_str("mode = \"live\"\n").is_err());
    }

    #[test]
    fn hard_cap_enforced_on_max_notional() {
        let mut c = Config::default();
        c.sizing.max_notional_usd = 10_000.01;
        let msgs = invalid_msgs(&c);
        assert!(msgs.iter().any(|m| m.contains("hard cap")), "{msgs:?}");
    }

    #[test]
    fn notional_above_cap_rejected() {
        let mut c = Config::default();
        c.sizing.notional_usd = 6000.0;
        c.sizing.max_notional_usd = 5000.0;
        let msgs = invalid_msgs(&c);
        assert!(
            msgs.iter()
                .any(|m| m.contains("exceeds sizing.max_notional_usd")),
            "{msgs:?}"
        );

        let mut c = Config::default();
        c.sizing.notional_usd = 20_000.0;
        c.sizing.max_notional_usd = 20_000.0;
        let msgs = invalid_msgs(&c);
        assert!(msgs.iter().filter(|m| m.contains("hard cap")).count() >= 2);
    }

    #[test]
    fn notional_at_cap_is_ok_and_effective_notional_clamped() {
        let mut c = Config::default();
        c.sizing.notional_usd = 10_000.0;
        c.validate().unwrap();
        assert_eq!(c.effective_notional_usd(), 10_000.0);

        // Even if validation were bypassed, the effective notional is clamped.
        let mut c = Config::default();
        c.sizing.notional_usd = 50_000.0;
        c.sizing.max_notional_usd = 50_000.0;
        assert_eq!(c.effective_notional_usd(), HARD_CAP_NOTIONAL_USD);
        c.sizing.max_notional_usd = 3000.0;
        assert_eq!(c.effective_notional_usd(), 3000.0);
    }

    #[test]
    fn leverage_bounds() {
        for (lev, ok) in [(0, false), (1, true), (2, true), (3, false), (10, false)] {
            let mut c = Config::default();
            c.sizing.leverage = lev;
            assert_eq!(c.validate().is_ok(), ok, "leverage {lev}");
        }
    }

    #[test]
    fn cross_margin_rejected() {
        let mut c = Config::default();
        c.sizing.margin_mode = MarginMode::Cross;
        assert!(invalid_msgs(&c).iter().any(|m| m.contains("isolated")));
    }

    #[test]
    fn thresholds_must_be_ordered() {
        let mut c = Config::default();
        c.strategy.exit_converge_bps = 50.0;
        assert!(invalid_msgs(&c)
            .iter()
            .any(|m| m.contains("exit_converge_bps")));

        let mut c = Config::default();
        c.risk.persistent_premium_bps = 40.0;
        assert!(invalid_msgs(&c)
            .iter()
            .any(|m| m.contains("persistent_premium_bps")));

        let mut c = Config::default();
        c.strategy.taker_fallback_window_secs = c.strategy.max_hold_secs;
        assert!(invalid_msgs(&c)
            .iter()
            .any(|m| m.contains("taker_fallback_window_secs")));
    }

    #[test]
    fn hit_rate_bounds() {
        for (v, ok) in [
            (0.0, false),
            (0.8, true),
            (1.0, true),
            (1.2, false),
            (f64::NAN, false),
        ] {
            let mut c = Config::default();
            c.risk.hit_rate_min = v;
            assert_eq!(c.validate().is_ok(), ok, "hit_rate_min {v}");
        }
        let mut c = Config::default();
        c.risk.hit_rate_window = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn loss_limits_must_be_positive() {
        let mut c = Config::default();
        c.risk.daily_loss_limit_usd = 0.0;
        c.risk.max_drawdown_usd = -1.0;
        let msgs = invalid_msgs(&c);
        assert!(msgs.iter().any(|m| m.contains("daily_loss_limit_usd")));
        assert!(msgs.iter().any(|m| m.contains("max_drawdown_usd")));
    }

    #[test]
    fn fee_sanity() {
        let mut c = Config::default();
        c.fees.taker_rate = 0.09; // typo: percent instead of fraction
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.fees.taker_rate = 0.00001;
        c.fees.maker_rate = 0.00003;
        assert!(c.validate().is_err());
    }

    #[test]
    fn coin_must_carry_dex_prefix() {
        let mut c = Config::default();
        c.market.coin = "ANT".into();
        assert!(invalid_msgs(&c).iter().any(|m| m.contains("prefixed")));
    }

    #[test]
    fn urls_and_timing_checked() {
        let mut c = Config::default();
        c.network.mainnet_ws_url = "ws://insecure".into();
        c.network.testnet_exchange_url = c.network.mainnet_exchange_url.clone();
        c.timing.ws_ping_secs = 60;
        c.timing.rest_refill_per_sec = 50.0;
        c.timing.meta_poll_secs = 0;
        let msgs = invalid_msgs(&c);
        assert_eq!(msgs.len(), 5, "{msgs:?}");
    }

    #[test]
    fn order_pacing_and_kill_flatten_checked() {
        let mut c = Config::default();
        c.strategy.max_order_actions_per_min = 0;
        c.strategy.min_order_interval_ms = 120_000;
        c.strategy.premium_avg_secs = 5 * 3600;
        c.risk.kill_flatten_maker_secs = c.strategy.max_hold_secs;
        let msgs = invalid_msgs(&c);
        assert_eq!(msgs.len(), 4, "{msgs:?}");
    }

    #[test]
    fn validation_reports_all_errors_at_once() {
        let mut c = Config::default();
        c.sizing.leverage = 5;
        c.sizing.notional_usd = -1.0;
        c.risk.hit_rate_min = 2.0;
        assert!(invalid_msgs(&c).len() >= 3);
    }

    #[test]
    fn endpoints_per_mode() {
        let c = Config::default();
        let p = c.endpoints(Mode::Paper);
        assert_eq!(p.exchange_url, None, "paper must never get an exchange URL");
        assert_eq!(p.info_url, "https://api.hyperliquid.xyz/info");
        assert!(p.is_mainnet);
        let t = c.endpoints(Mode::Testnet);
        assert!(!t.is_mainnet);
        assert!(t.info_url.contains("testnet"));
        assert!(t.exchange_url.unwrap().contains("testnet"));
        let m = c.endpoints(Mode::Mainnet);
        assert!(m.is_mainnet);
        assert_eq!(
            m.exchange_url.as_deref(),
            Some("https://api.hyperliquid.xyz/exchange")
        );
    }

    #[test]
    fn mode_from_str() {
        assert_eq!("Paper".parse::<Mode>().unwrap(), Mode::Paper);
        assert_eq!("testnet".parse::<Mode>().unwrap(), Mode::Testnet);
        assert_eq!("mainnet".parse::<Mode>().unwrap(), Mode::Mainnet);
        assert!("prod".parse::<Mode>().is_err());
    }

    #[test]
    fn load_missing_file_is_io_error() {
        let err = Config::load("/nonexistent/antbot.toml").unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn load_from_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, toml::to_string(&Config::default()).unwrap()).unwrap();
        assert_eq!(Config::load(&p).unwrap(), Config::default());
    }
}
