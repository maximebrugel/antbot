//! Persisted bot state (strategy state, PnL ledger, equity curve, trade
//! history, executor state) as JSON in the data dir, resumed on restart.
//!
//! One file per mode (`state_paper.json`, `state_testnet.json`, ...) so a
//! paper run never resumes testnet orders and vice versa. Writes are atomic
//! (temp file + rename). The risk / kill state keeps its own file
//! (`risk_state.json`, see [`crate::risk::kill`]) because the
//! `kill-status` / `reset-kill` commands operate on it.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::Mode;
use crate::strategy::signal::{StrategyState, TradeRecord};

pub const STATE_VERSION: u32 = 1;
/// Max equity points kept (older ones are thinned out).
pub const EQUITY_CURVE_MAX: usize = 20_000;

pub fn state_path(data_dir: &Path, mode: Mode) -> PathBuf {
    data_dir.join(format!("state_{mode}.json"))
}

/// The engine's own ledger (strategy-level accounting).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Ledger {
    /// Sum of closed trades' net PnL (USD).
    pub realized_pnl_usd: f64,
    pub trades: Vec<TradeRecord>,
    /// (time ms, equity USD) where equity = realized + unrealized.
    pub equity_curve: Vec<(u64, f64)>,
}

impl Ledger {
    pub fn record_trade(&mut self, t: &TradeRecord) {
        self.realized_pnl_usd += t.pnl_usd;
        self.trades.push(t.clone());
    }

    /// Append an equity point, thinning the curve when it gets too long.
    pub fn push_equity(&mut self, now_ms: u64, equity: f64) {
        if !equity.is_finite() {
            return;
        }
        self.equity_curve.push((now_ms, equity));
        if self.equity_curve.len() > EQUITY_CURVE_MAX {
            // Keep every other point of the older half, the recent half intact.
            let half = self.equity_curve.len() / 2;
            let mut thinned: Vec<(u64, f64)> = self.equity_curve[..half]
                .iter()
                .step_by(2)
                .copied()
                .collect();
            thinned.extend_from_slice(&self.equity_curve[half..]);
            self.equity_curve = thinned;
        }
    }
}

/// Everything persisted besides the risk state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotState {
    pub version: u32,
    pub mode: Mode,
    pub coin: String,
    pub saved_ms: u64,
    pub strategy: StrategyState,
    pub ledger: Ledger,
    /// Executor-specific state (paper account and orders / live order map).
    #[serde(default)]
    pub executor: serde_json::Value,
    /// Premium rolling-mean samples (slot start ms, premium bps), so a
    /// restart does not restart the averaging warm-up.
    #[serde(default)]
    pub premium_samples: Vec<(u64, f64)>,
}

impl BotState {
    pub fn load(path: &Path) -> anyhow::Result<Option<BotState>> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let st: BotState = serde_json::from_str(&text)
                    .with_context(|| format!("corrupt state file {}", path.display()))?;
                if st.version != STATE_VERSION {
                    anyhow::bail!(
                        "state file {} has version {} (expected {STATE_VERSION})",
                        path.display(),
                        st.version
                    );
                }
                Ok(Some(st))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        save_json_atomic(path, self)
    }
}

/// Atomic JSON write: temp file in the same dir, fsync, rename.
pub fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = state_path(dir.path(), Mode::Paper);
        assert!(p.ends_with("state_paper.json"));
        assert!(BotState::load(&p).unwrap().is_none());
        let mut st = BotState {
            version: STATE_VERSION,
            mode: Mode::Paper,
            coin: "xyz:ANT".into(),
            saved_ms: 1,
            strategy: StrategyState::default(),
            ledger: Ledger::default(),
            executor: serde_json::json!({"orders": []}),
            premium_samples: vec![(1, 2.0)],
        };
        st.ledger.push_equity(5, 1.5);
        st.save(&p).unwrap();
        assert_eq!(BotState::load(&p).unwrap().unwrap(), st);
        std::fs::write(&p, "{not json").unwrap();
        assert!(BotState::load(&p).is_err(), "corrupt file is an error");
    }

    #[test]
    fn equity_curve_is_thinned() {
        let mut l = Ledger::default();
        for i in 0..(EQUITY_CURVE_MAX as u64 + 1) {
            l.push_equity(i, i as f64);
        }
        assert!(l.equity_curve.len() <= EQUITY_CURVE_MAX);
        assert_eq!(l.equity_curve.last().unwrap().0, EQUITY_CURVE_MAX as u64);
        assert!(l.equity_curve.windows(2).all(|w| w[0].0 < w[1].0));
        l.push_equity(1, f64::NAN);
        assert_eq!(l.equity_curve.last().unwrap().0, EQUITY_CURVE_MAX as u64);
    }
}
