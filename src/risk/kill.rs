//! Kill switch rules (pure, deterministic; time and values are passed in).
//!
//! | rule | trigger |
//! |---|---|
//! | hit rate | over the last `hit_rate_window` (N) closed trades, misses > N·(1 − `hit_rate_min`). That is "hit rate < min over the last N trades". It also fires early, before N trades exist, once the misses already make that outcome certain |
//! | persistent premium | \|premium\| > `persistent_premium_bps` continuously for `persistent_premium_secs`. Missing samples do not reset the timer; a valid sample at or below the threshold does |
//! | spec change | any watched field differs from the baseline snapshot ([`super::spec`]), the baseline disagrees with `[market] expected_*`, or the coin is gone from meta |
//! | daily loss | equity − equity at the start of the UTC day ≤ −`daily_loss_limit_usd` |
//! | max drawdown | peak equity − equity ≥ `max_drawdown_usd` |
//!
//! "Equity" is the strategy's cumulative PnL in USD (realized net of fees and
//! funding, plus unrealized at mark). Only differences matter, so any
//! consistent origin works.
//!
//! A kill returns a [`KillOrder`]: cancel all orders, flatten if
//! `flatten_on_kill`, stop new entries, alert. The kill is sticky: it is
//! stored in [`RiskState`], persisted to `<data_dir>/risk_state.json`, and
//! only [`RiskManager::reset`] (CLI `antbot reset-kill --confirm`) clears it.
//! Later triggers while killed are recorded but produce no new order.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::spec::WatchedSpec;
use crate::config::{HitDefinition, MarketConfig, RiskConfig};
use crate::hl::assets::AssetSpec;
use crate::strategy::signal::TradeRecord;

/// File name of the persisted risk state inside the data dir.
pub const RISK_STATE_FILE: &str = "risk_state.json";
const MS_PER_DAY: u64 = 86_400_000;
const MAX_EXTRA_REASONS: usize = 32;

/// Path of the risk state file for a data dir.
pub fn risk_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RISK_STATE_FILE)
}

/// UTC day number (days since the Unix epoch).
pub fn utc_day(now_ms: u64) -> u64 {
    now_ms / MS_PER_DAY
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum KillReason {
    HitRate {
        hits: usize,
        trades: usize,
        window: usize,
        min_rate: f64,
    },
    PersistentPremium {
        premium_bps: f64,
        threshold_bps: f64,
        since_ms: u64,
        duration_secs: u64,
    },
    SpecChange {
        changes: Vec<String>,
    },
    DailyLoss {
        utc_day: u64,
        pnl_usd: f64,
        limit_usd: f64,
    },
    MaxDrawdown {
        peak_usd: f64,
        equity_usd: f64,
        drawdown_usd: f64,
        limit_usd: f64,
    },
    Manual {
        note: String,
    },
}

impl KillReason {
    pub fn rule(&self) -> &'static str {
        match self {
            KillReason::HitRate { .. } => "hit_rate",
            KillReason::PersistentPremium { .. } => "persistent_premium",
            KillReason::SpecChange { .. } => "spec_change",
            KillReason::DailyLoss { .. } => "daily_loss",
            KillReason::MaxDrawdown { .. } => "max_drawdown",
            KillReason::Manual { .. } => "manual",
        }
    }

    /// One-line English description for logs and alerts.
    pub fn describe(&self) -> String {
        match self {
            KillReason::HitRate {
                hits,
                trades,
                window,
                min_rate,
            } => format!(
                "hit rate {hits}/{trades} over the last {window} trades is below {:.0}%",
                min_rate * 100.0
            ),
            KillReason::PersistentPremium {
                premium_bps,
                threshold_bps,
                duration_secs,
                ..
            } => format!(
                "|premium| above {threshold_bps:.0} bps for {duration_secs} s (now {premium_bps:.1} bps)"
            ),
            KillReason::SpecChange { changes } => {
                format!("market spec changed: {}", changes.join("; "))
            }
            KillReason::DailyLoss {
                pnl_usd, limit_usd, ..
            } => format!("daily PnL {pnl_usd:.2} USD hit the -{limit_usd:.2} USD limit (UTC day)"),
            KillReason::MaxDrawdown {
                drawdown_usd,
                limit_usd,
                peak_usd,
                ..
            } => format!(
                "drawdown {drawdown_usd:.2} USD from peak {peak_usd:.2} USD hit the {limit_usd:.2} USD limit"
            ),
            KillReason::Manual { note } => format!("manual kill: {note}"),
        }
    }
}

/// The persisted kill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KillRecord {
    pub reason: KillReason,
    pub at_ms: u64,
    pub message: String,
    /// Triggers seen after the kill (not acted on).
    #[serde(default)]
    pub later: Vec<(u64, KillReason)>,
}

/// What the caller must do on a kill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KillOrder {
    pub reason: KillReason,
    pub cancel_orders: bool,
    pub flatten: bool,
    pub stop_entries: bool,
    pub alert: String,
}

/// Result of a spec check.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SpecCheck {
    /// Differences vs the baseline / expected config (empty = unchanged).
    pub changes: Vec<String>,
    /// Some when the change kills (`kill_on_spec_change`).
    pub kill: Option<KillOrder>,
}

/// Persisted risk state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RiskState {
    pub kill: Option<KillRecord>,
    /// Outcomes of the last N closed trades (true = hit), oldest first.
    pub outcomes: VecDeque<bool>,
    pub trades_closed: u64,
    pub peak_equity_usd: Option<f64>,
    pub day: Option<u64>,
    pub day_start_equity_usd: Option<f64>,
    pub last_equity_usd: Option<f64>,
    pub premium_above_since_ms: Option<u64>,
    pub baseline_spec: Option<WatchedSpec>,
}

impl RiskState {
    /// Load from `path`; `Ok(None)` if the file does not exist.
    pub fn load(path: &Path) -> anyhow::Result<Option<RiskState>> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(serde_json::from_str(&s).map_err(|e| {
                anyhow::anyhow!("corrupt risk state {}: {e}", path.display())
            })?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
        }
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Risk parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskParams {
    pub hit_rate_window: usize,
    pub hit_rate_min: f64,
    pub hit_definition: HitDefinition,
    pub persistent_premium_bps: f64,
    pub persistent_premium_ms: u64,
    pub kill_on_spec_change: bool,
    pub daily_loss_limit_usd: f64,
    pub max_drawdown_usd: f64,
    pub flatten_on_kill: bool,
}

impl From<&RiskConfig> for RiskParams {
    fn from(r: &RiskConfig) -> Self {
        Self {
            hit_rate_window: r.hit_rate_window,
            hit_rate_min: r.hit_rate_min,
            hit_definition: r.hit_definition,
            persistent_premium_bps: r.persistent_premium_bps,
            persistent_premium_ms: r.persistent_premium_secs * 1000,
            kill_on_spec_change: r.kill_on_spec_change,
            daily_loss_limit_usd: r.daily_loss_limit_usd,
            max_drawdown_usd: r.max_drawdown_usd,
            flatten_on_kill: r.flatten_on_kill,
        }
    }
}

/// Whether a closed trade counts as a hit under `def`.
pub fn is_hit(def: HitDefinition, t: &TradeRecord) -> bool {
    match def {
        HitDefinition::PremiumTowardZero => t.moved_toward_zero(),
        HitDefinition::PnlPositive => t.pnl_usd > 0.0,
    }
}

/// Whether a closed trade says anything about the strategy for the hit-rate
/// rule. With the premium definition, a trade whose entry or exit premium is
/// unknown (a position adopted at startup, an external close without data)
/// is not counted, neither as a hit nor as a miss.
pub fn counts_for_hit_rate(def: HitDefinition, t: &TradeRecord) -> bool {
    match def {
        HitDefinition::PremiumTowardZero => {
            t.entry_premium_bps.is_some_and(f64::is_finite)
                && t.exit_premium_bps.is_some_and(f64::is_finite)
        }
        HitDefinition::PnlPositive => true,
    }
}

#[derive(Debug, Clone)]
pub struct RiskManager {
    p: RiskParams,
    s: RiskState,
}

impl RiskManager {
    pub fn new(params: RiskParams, state: RiskState) -> Self {
        let mut m = Self {
            p: params,
            s: state,
        };
        // A smaller window in a new config: keep only the last N outcomes.
        while m.s.outcomes.len() > m.p.hit_rate_window {
            m.s.outcomes.pop_front();
        }
        m
    }

    pub fn state(&self) -> &RiskState {
        &self.s
    }

    pub fn params(&self) -> &RiskParams {
        &self.p
    }

    pub fn is_killed(&self) -> bool {
        self.s.kill.is_some()
    }

    pub fn kill_record(&self) -> Option<&KillRecord> {
        self.s.kill.as_ref()
    }

    /// New entries are allowed only while not killed.
    pub fn entries_allowed(&self) -> bool {
        !self.is_killed()
    }

    /// Hits / trades in the current window.
    pub fn hit_rate(&self) -> Option<(usize, usize)> {
        let n = self.s.outcomes.len();
        (n > 0).then(|| (self.s.outcomes.iter().filter(|h| **h).count(), n))
    }

    /// PnL since the start of the current UTC day.
    pub fn daily_pnl(&self) -> Option<f64> {
        Some(self.s.last_equity_usd? - self.s.day_start_equity_usd?)
    }

    pub fn drawdown(&self) -> Option<f64> {
        Some(self.s.peak_equity_usd? - self.s.last_equity_usd?)
    }

    fn kill(&mut self, now_ms: u64, reason: KillReason) -> Option<KillOrder> {
        if let Some(k) = self.s.kill.as_mut() {
            if k.later.len() < MAX_EXTRA_REASONS {
                k.later.push((now_ms, reason));
            }
            return None;
        }
        let message = format!("KILL SWITCH [{}]: {}", reason.rule(), reason.describe());
        self.s.kill = Some(KillRecord {
            reason: reason.clone(),
            at_ms: now_ms,
            message: message.clone(),
            later: Vec::new(),
        });
        Some(KillOrder {
            reason,
            cancel_orders: true,
            flatten: self.p.flatten_on_kill,
            stop_entries: true,
            alert: message,
        })
    }

    /// Manual kill (operator / fatal error).
    pub fn trigger_manual(&mut self, now_ms: u64, note: &str) -> Option<KillOrder> {
        self.kill(
            now_ms,
            KillReason::Manual {
                note: note.to_string(),
            },
        )
    }

    /// Record a closed trade and apply the hit-rate rule.
    pub fn on_trade_closed(&mut self, now_ms: u64, t: &TradeRecord) -> Option<KillOrder> {
        self.s.trades_closed += 1;
        if !counts_for_hit_rate(self.p.hit_definition, t) {
            return None;
        }
        let hit = is_hit(self.p.hit_definition, t);
        self.s.outcomes.push_back(hit);
        while self.s.outcomes.len() > self.p.hit_rate_window {
            self.s.outcomes.pop_front();
        }
        let n = self.s.outcomes.len();
        let hits = self.s.outcomes.iter().filter(|h| **h).count();
        let misses = n - hits;
        let window = self.p.hit_rate_window;
        let allowed_misses = window as f64 * (1.0 - self.p.hit_rate_min);
        if misses as f64 > allowed_misses + 1e-9 {
            return self.kill(
                now_ms,
                KillReason::HitRate {
                    hits,
                    trades: n,
                    window,
                    min_rate: self.p.hit_rate_min,
                },
            );
        }
        None
    }

    /// Feed a premium sample (bps, `None` = unavailable / stale).
    pub fn on_premium(&mut self, now_ms: u64, premium_bps: Option<f64>) -> Option<KillOrder> {
        let p = premium_bps.filter(|p| p.is_finite())?;
        if p.abs() <= self.p.persistent_premium_bps {
            self.s.premium_above_since_ms = None;
            return None;
        }
        let since = *self.s.premium_above_since_ms.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) >= self.p.persistent_premium_ms {
            return self.kill(
                now_ms,
                KillReason::PersistentPremium {
                    premium_bps: p,
                    threshold_bps: self.p.persistent_premium_bps,
                    since_ms: since,
                    duration_secs: self.p.persistent_premium_ms / 1000,
                },
            );
        }
        None
    }

    /// Check the live spec. The first call (no baseline yet) validates it
    /// against `[market] expected_*` and stores it as the baseline.
    pub fn on_spec(&mut self, now_ms: u64, live: &AssetSpec, expected: &MarketConfig) -> SpecCheck {
        let w = WatchedSpec::from(live);
        let mut changes = Vec::new();
        match &self.s.baseline_spec {
            None => {
                changes.extend(
                    live.check_expected(expected)
                        .into_iter()
                        .map(|c| format!("baseline vs config: {c}")),
                );
                self.s.baseline_spec = Some(w);
            }
            Some(base) => changes.extend(base.diff(&w)),
        }
        self.spec_outcome(now_ms, changes, Some(WatchedSpec::from(live)))
    }

    /// The coin could not be found in meta any more.
    pub fn on_spec_unavailable(&mut self, now_ms: u64, what: &str) -> SpecCheck {
        self.spec_outcome(now_ms, vec![format!("asset unavailable: {what}")], None)
    }

    fn spec_outcome(
        &mut self,
        now_ms: u64,
        changes: Vec<String>,
        new_baseline: Option<WatchedSpec>,
    ) -> SpecCheck {
        if changes.is_empty() {
            return SpecCheck::default();
        }
        let kill = if self.p.kill_on_spec_change {
            self.kill(
                now_ms,
                KillReason::SpecChange {
                    changes: changes.clone(),
                },
            )
        } else {
            // Warn once per change: accept the new spec as the baseline.
            if let Some(b) = new_baseline {
                self.s.baseline_spec = Some(b);
            }
            None
        };
        SpecCheck { changes, kill }
    }

    /// Feed the current equity (USD): daily loss and drawdown rules.
    pub fn on_equity(&mut self, now_ms: u64, equity_usd: f64) -> Option<KillOrder> {
        if !equity_usd.is_finite() {
            return None;
        }
        let day = utc_day(now_ms);
        if self.s.day != Some(day) || self.s.day_start_equity_usd.is_none() {
            self.s.day = Some(day);
            // The day starts from the last equity seen (previous day's close)
            // so a loss between two samples across midnight still counts;
            // on the very first sample it starts from the current equity.
            self.s.day_start_equity_usd = Some(self.s.last_equity_usd.unwrap_or(equity_usd));
        }
        self.s.last_equity_usd = Some(equity_usd);
        let peak = self
            .s
            .peak_equity_usd
            .map_or(equity_usd, |p| p.max(equity_usd));
        self.s.peak_equity_usd = Some(peak);

        let day_pnl = equity_usd - self.s.day_start_equity_usd.unwrap_or(equity_usd);
        if day_pnl <= -self.p.daily_loss_limit_usd {
            return self.kill(
                now_ms,
                KillReason::DailyLoss {
                    utc_day: day,
                    pnl_usd: day_pnl,
                    limit_usd: self.p.daily_loss_limit_usd,
                },
            );
        }
        let dd = peak - equity_usd;
        if dd >= self.p.max_drawdown_usd {
            return self.kill(
                now_ms,
                KillReason::MaxDrawdown {
                    peak_usd: peak,
                    equity_usd,
                    drawdown_usd: dd,
                    limit_usd: self.p.max_drawdown_usd,
                },
            );
        }
        None
    }

    /// Manual reset: clear the kill and the conditions that would re-fire
    /// immediately (hit-rate window, drawdown peak, daily start, premium
    /// timer). The spec baseline is cleared too, so the next poll re-checks
    /// against the config and takes a fresh snapshot. Returns the cleared kill.
    pub fn reset(&mut self) -> Option<KillRecord> {
        let k = self.s.kill.take();
        reset_conditions(&mut self.s);
        k
    }
}

/// Clear the kill and its re-firing conditions in a persisted state (used by
/// the `reset-kill` CLI, which has no running manager).
pub fn reset_state(s: &mut RiskState) -> Option<KillRecord> {
    let k = s.kill.take();
    reset_conditions(s);
    k
}

fn reset_conditions(s: &mut RiskState) {
    s.outcomes.clear();
    s.peak_equity_usd = None;
    s.day = None;
    s.day_start_equity_usd = None;
    s.premium_above_since_ms = None;
    s.baseline_spec = None;
}

#[cfg(test)]
mod tests;
