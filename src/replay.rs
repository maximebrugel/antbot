//! Replay / backtest: the live [`Engine`] (strategy + risk) and the paper
//! fill model ([`PaperSim`]) driven by historical hourly data.
//!
//! Inputs (read-only):
//! * the hourly `fundingHistory` of the coin (premium = impact mid vs oracle,
//!   the backtest signal; funding rate);
//! * 1h `candleSnapshot` candles.
//!
//! Either from files (`--input DIR` with `candles_1h.json` and
//! `funding_history.json` / `api_funding_history.json`, in DIR or its parent,
//! in the research format: a map coin -> entries, or a plain array), or
//! fetched from the mainnet info API (`--fetch --days N`, read-only).
//!
//! Market reconstruction, hour `h` (funding stamps floored to the hour,
//! first entry per hour, as in the research `backtest.py`):
//! * price at `h` = close of the candle ending at `h` (the backtest's
//!   `px_at`); book = that price -/+ half of `spread_bps`, rounded to the
//!   tick, with synthetic depth;
//! * oracle = price / (1 + premium[h]), impact bid/ask = book, so the
//!   strategy's impact premium equals the historical hourly premium;
//! * funding rate[h] is settled by the paper funding clock at hour
//!   boundaries.
//!
//! Within the hour the engine is ticked every `step_secs` (time-based rules:
//! timeouts, requotes, max-hold schedule, taker fallback). Public trades are
//! synthesised from the candle starting at `h`: its low (seller aggressor)
//! and high (buyer aggressor), each half the candle volume, at `h + 5 min`
//! and `h + 45 min`. A resting ALO therefore fills, as maker at its own
//! price, when the hour's range trades through (or touches) it — the paper
//! fill model, applied to hourly extremes. This is an approximation:
//! intra-hour ordering of the low and the high is unknown.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde_json::Value;

use crate::config::Config;
use crate::engine::{Engine, EngineParams, Notice};
use crate::exec::paper::{PaperParams, PaperSim};
use crate::exec::state::Ledger;
use crate::exec::ExecEvent;
use crate::hl::market::{DataSource, MarketState};
use crate::hl::rounding::{round_price, RoundDir};
use crate::hl::types::{Candle, FundingHistoryEntry, L2Book, Level, WsTrade};
use crate::risk::kill::is_hit;
use crate::risk::{RiskManager, RiskParams, RiskState};
use crate::strategy::premium::{basis_return, to_bps};
use crate::strategy::signal::{Command, ExitReason, Strategy, StrategyParams, TradeRecord};

const HOUR_MS: u64 = 3_600_000;
const MAX_PUMP_ROUNDS: usize = 32;

/// One hour of history.
#[derive(Debug, Clone, PartialEq)]
pub struct HourBar {
    /// Hour stamp (ms, floored).
    pub t: u64,
    /// Hourly premium (fraction).
    pub premium: f64,
    pub funding_rate: f64,
    /// Price at `t`: close of the candle ending at `t`.
    pub px: f64,
    /// Candle starting at `t` (its range drives the fills), if known.
    pub next: Option<Candle>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayData {
    pub coin: String,
    pub bars: Vec<HourBar>,
}

impl ReplayData {
    /// Join funding history (hour-floored, first entry per hour) with
    /// candles. Hours without a price are skipped.
    pub fn build(coin: &str, funding: &[FundingHistoryEntry], candles: &[Candle]) -> Self {
        let mut prem: BTreeMap<u64, (f64, f64)> = BTreeMap::new();
        let mut fs: Vec<&FundingHistoryEntry> = funding.iter().filter(|f| f.coin == coin).collect();
        fs.sort_by_key(|f| f.time);
        for f in fs {
            prem.entry((f.time / HOUR_MS) * HOUR_MS)
                .or_insert((f.premium, f.funding_rate));
        }
        let mut by_start: BTreeMap<u64, Candle> = BTreeMap::new();
        for c in candles {
            by_start.entry(c.t).or_insert_with(|| c.clone());
        }
        let bars = prem
            .into_iter()
            .filter_map(|(t, (p, fr))| {
                let px = by_start.get(&t.checked_sub(HOUR_MS)?)?.c;
                Some(HourBar {
                    t,
                    premium: p,
                    funding_rate: fr,
                    px,
                    next: by_start.get(&t).cloned(),
                })
            })
            .filter(|b| b.px.is_finite() && b.px > 0.0 && b.premium.is_finite())
            .collect();
        Self {
            coin: coin.to_string(),
            bars,
        }
    }

    /// Load from a directory (see the module docs for the file names).
    pub fn load_dir(dir: &Path, coin: &str) -> anyhow::Result<Self> {
        let find = |names: &[&str]| -> Option<PathBuf> {
            let mut dirs = vec![dir.to_path_buf()];
            if let Some(p) = dir.parent() {
                dirs.push(p.to_path_buf());
            }
            dirs.iter()
                .flat_map(|d| names.iter().map(move |n| d.join(n)))
                .find(|p| p.is_file())
        };
        let cpath = find(&["candles_1h.json"])
            .ok_or_else(|| anyhow!("no candles_1h.json in {} (or its parent)", dir.display()))?;
        let fpath =
            find(&["funding_history.json", "api_funding_history.json"]).ok_or_else(|| {
                anyhow!(
                    "no funding_history.json / api_funding_history.json in {} (or its parent)",
                    dir.display()
                )
            })?;
        Self::load_files(&cpath, &fpath, coin)
    }

    pub fn load_files(candles: &Path, funding: &Path, coin: &str) -> anyhow::Result<Self> {
        let read = |p: &Path| -> anyhow::Result<Value> {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))
        };
        let c: Vec<Candle> = serde_json::from_value(coin_entries(read(candles)?, coin)?)
            .context("decoding candles")?;
        let f: Vec<FundingHistoryEntry> =
            serde_json::from_value(coin_entries(read(funding)?, coin)?)
                .context("decoding funding history")?;
        Ok(Self::build(coin, &f, &c))
    }

    /// Fetch `days` of history from the info API (read-only).
    pub async fn fetch(
        info: &crate::hl::info::InfoClient,
        coin: &str,
        days: u64,
        now_ms: u64,
    ) -> anyhow::Result<(Self, Vec<FundingHistoryEntry>, Vec<Candle>)> {
        let start = now_ms.saturating_sub(days * 24 * HOUR_MS);
        let candles = info
            .candle_snapshot(coin, "1h", start.saturating_sub(HOUR_MS), now_ms)
            .await
            .context("candleSnapshot")?;
        // fundingHistory pages at 500 entries: walk forward.
        let mut funding: Vec<FundingHistoryEntry> = Vec::new();
        let mut from = start;
        for _ in 0..100 {
            let page = info
                .funding_history(coin, from, Some(now_ms))
                .await
                .context("fundingHistory")?;
            let Some(last) = page.iter().map(|f| f.time).max() else {
                break;
            };
            let n = page.len();
            funding.extend(page);
            if n < 500 || last < from {
                break;
            }
            from = last + 1;
        }
        let data = Self::build(coin, &funding, &candles);
        Ok((data, funding, candles))
    }
}

/// Research files are maps coin -> entries; plain arrays are accepted too.
fn coin_entries(v: Value, coin: &str) -> anyhow::Result<Value> {
    match v {
        Value::Array(_) => Ok(v),
        Value::Object(mut m) => m
            .remove(coin)
            .ok_or_else(|| anyhow!("{coin} not found in the input file")),
        _ => Err(anyhow!(
            "unexpected JSON shape (want an array or a coin map)"
        )),
    }
}

/// Replay knobs.
#[derive(Debug, Clone)]
pub struct ReplayParams {
    pub sz_decimals: u32,
    pub max_leverage: u32,
    /// Synthetic book spread (bps of the price).
    pub spread_bps: f64,
    /// Synthetic depth per level (USD).
    pub depth_usd: f64,
    /// Engine tick inside the hour.
    pub step_secs: u64,
}

impl Default for ReplayParams {
    fn default() -> Self {
        Self {
            sz_decimals: 2,
            max_leverage: 10,
            spread_bps: 2.0,
            depth_usd: 25_000.0,
            step_secs: 60,
        }
    }
}

/// What came out of a replay.
#[derive(Debug, Clone)]
pub struct ReplayResult {
    pub trades: Vec<TradeRecord>,
    pub ledger: Ledger,
    pub kills: Vec<(u64, String)>,
    pub orders_placed: u64,
    pub post_only_rejects: u64,
    pub entries_aborted: u64,
    pub hours: usize,
    pub first_ms: u64,
    pub last_ms: u64,
    /// Simulated account net PnL (cross-check of the strategy ledger).
    pub account_net_pnl_usd: f64,
}

/// Synthetic market at time `now` for bar `b`.
fn market(coin: &str, b: &HourBar, now: u64, p: &ReplayParams) -> MarketState {
    let half = p.spread_bps * 1e-4 / 2.0;
    let mut bid = round_price(b.px * (1.0 - half), p.sz_decimals, RoundDir::Down).unwrap_or(b.px);
    let mut ask = round_price(b.px * (1.0 + half), p.sz_decimals, RoundDir::Up).unwrap_or(b.px);
    if bid >= ask {
        // Spread below one tick: one tick around the price.
        let tick = 10f64.powi(-((6 - p.sz_decimals as i32).max(0)));
        bid = round_price(b.px - tick / 2.0, p.sz_decimals, RoundDir::Down).unwrap_or(b.px - tick);
        ask = round_price(b.px + tick / 2.0, p.sz_decimals, RoundDir::Up).unwrap_or(b.px + tick);
    }
    let lvl_sz = p.depth_usd / b.px;
    let step = b.px * 1e-4; // 1 bp between levels
    let bids = (0..5)
        .map(|i| Level {
            px: bid - step * i as f64,
            sz: lvl_sz,
            n: 1,
        })
        .collect();
    let asks = (0..5)
        .map(|i| Level {
            px: ask + step * i as f64,
            sz: lvl_sz,
            n: 1,
        })
        .collect();
    let mut m = MarketState::new(coin);
    m.apply_book(
        &L2Book {
            coin: coin.into(),
            time: now,
            levels: (bids, asks),
        },
        now,
        DataSource::Rest,
    );
    let mid = 0.5 * (bid + ask);
    let oracle = mid / (1.0 + b.premium);
    m.oracle_px = Some(oracle);
    m.mark_px = Some(mid);
    m.mid_px = Some(mid);
    m.impact_bid_px = Some(bid);
    m.impact_ask_px = Some(ask);
    m.api_premium = Some(b.premium);
    m.funding = Some(b.funding_rate);
    m.ctx_recv_ms = Some(now);
    m.ctx_source = DataSource::Rest;
    m
}

/// Trades synthesised from a candle: low (seller) and high (buyer), twice.
fn candle_trades(coin: &str, c: &Candle, h: u64) -> Vec<(u64, WsTrade)> {
    let vol = if c.v.is_finite() && c.v > 0.0 {
        c.v
    } else {
        0.0
    };
    if vol == 0.0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut tid = h; // unique per hour
    for off in [5 * 60_000, 45 * 60_000] {
        for (px, side) in [(c.l, "A"), (c.h, "B")] {
            tid += 1;
            out.push((
                h + off,
                WsTrade {
                    coin: coin.into(),
                    side: side.into(),
                    px,
                    sz: vol / 4.0,
                    time: h + off,
                    tid,
                },
            ));
        }
    }
    out
}

struct Sim<'a> {
    engine: Engine,
    sim: PaperSim,
    res: &'a mut ReplayResult,
}

impl Sim<'_> {
    fn feed(&mut self, now: u64, evs: Vec<ExecEvent>, m: &MarketState) {
        let mut cmds = Vec::new();
        for ev in evs {
            cmds.extend(self.engine.on_exec(now, ev));
        }
        self.pump(now, m, cmds);
    }

    fn pump(&mut self, now: u64, m: &MarketState, mut cmds: Vec<Command>) {
        for _ in 0..MAX_PUMP_ROUNDS {
            if cmds.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for c in std::mem::take(&mut cmds) {
                if matches!(c, Command::Place(_)) {
                    self.res.orders_placed += 1;
                }
                for ev in self.sim.execute(now, c, m) {
                    next.extend(self.engine.on_exec(now, ev));
                }
            }
            cmds = next;
        }
        self.notices();
    }

    fn tick(&mut self, now: u64, m: &MarketState) {
        let evs = self.sim.on_book(now, m);
        self.feed(now, evs, m);
        let margin = Some(self.sim.available_margin(m.mark_px));
        let cmds = self.engine.on_market(now, m, margin);
        self.pump(now, m, cmds);
    }

    fn notices(&mut self) {
        use crate::strategy::signal::{DoneReason, Event};
        for n in self.engine.drain_notices() {
            match n {
                Notice::Kill(t, k) => self.res.kills.push((t, k.alert)),
                Notice::Strategy(_, Event::EntryAborted { .. }) => self.res.entries_aborted += 1,
                Notice::Strategy(
                    _,
                    Event::OrderRejected {
                        reason: DoneReason::PostOnlyRejected,
                        ..
                    },
                ) => self.res.post_only_rejects += 1,
                _ => {}
            }
        }
    }
}

/// Run the replay.
pub fn run(cfg: &Config, data: &ReplayData, p: &ReplayParams) -> anyhow::Result<ReplayResult> {
    if data.bars.is_empty() {
        anyhow::bail!("no usable hours in the replay data");
    }
    let engine = Engine::new(
        EngineParams::from_config(cfg),
        Strategy::new(StrategyParams::from_config(
            cfg,
            p.sz_decimals,
            p.max_leverage,
        )),
        RiskManager::new(RiskParams::from(&cfg.risk), RiskState::default()),
        Ledger::default(),
        // The replay's premium already is the hourly fundingHistory mean:
        // averaging it again would only add lag.
        0,
    );
    let sim = PaperSim::new(PaperParams::from_config(cfg, p.sz_decimals));
    let mut res = ReplayResult {
        trades: Vec::new(),
        ledger: Ledger::default(),
        kills: Vec::new(),
        orders_placed: 0,
        post_only_rejects: 0,
        entries_aborted: 0,
        hours: data.bars.len(),
        first_ms: data.bars[0].t,
        last_ms: data.bars[data.bars.len() - 1].t,
        account_net_pnl_usd: 0.0,
    };
    let mut s = Sim {
        engine,
        sim,
        res: &mut res,
    };
    let step = p.step_secs.clamp(1, 3600) * 1000;
    for (i, b) in data.bars.iter().enumerate() {
        // Only simulate inside the hour if the next bar is contiguous (gaps
        // in the data would otherwise stretch a stale price).
        let contiguous = data.bars.get(i + 1).is_some_and(|n| n.t == b.t + HOUR_MS);
        let mut trades = match (&b.next, contiguous) {
            (Some(c), true) => candle_trades(&data.coin, c, b.t),
            _ => Vec::new(),
        };
        trades.sort_by_key(|(t, _)| *t);
        let mut ti = 0;
        let end = if contiguous { b.t + HOUR_MS } else { b.t + 1 };
        let mut now = b.t;
        while now < end {
            s.tick(now, &market(&data.coin, b, now, p));
            let next_t = now + step;
            while ti < trades.len() && trades[ti].0 < next_t.min(end) {
                let (t, tr) = &trades[ti];
                let m = market(&data.coin, b, *t, p);
                let evs = s.sim.on_trades(std::slice::from_ref(tr));
                s.feed(*t, evs, &m);
                ti += 1;
            }
            now = next_t;
        }
    }
    let last = data.bars.last().map(|b| b.px);
    let Sim { engine, sim, .. } = s;
    res.account_net_pnl_usd = sim.account().net_pnl(last);
    res.trades = engine.ledger.trades.clone();
    res.ledger = engine.ledger;
    Ok(res)
}

/// Summary statistics of a set of trades.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Stats {
    pub n: usize,
    pub hit_rate_pnl: f64,
    pub hit_rate_toward_zero: f64,
    pub mean_bps: f64,
    pub median_bps: f64,
    pub mean_basis_bps: f64,
    pub total_pnl_usd: f64,
    pub fees_usd: f64,
    pub funding_usd: f64,
    pub max_drawdown_usd: f64,
    pub mean_hold_h: f64,
    pub converged_exits: usize,
    pub max_hold_exits: usize,
    pub kill_exits: usize,
    pub taker_exits: usize,
    pub worst_bps: f64,
    pub best_bps: f64,
}

pub fn stats(trades: &[TradeRecord]) -> Stats {
    let n = trades.len();
    let nf = n.max(1) as f64;
    let mut bps: Vec<f64> = trades.iter().map(|t| t.pnl_bps).collect();
    bps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if n == 0 {
        0.0
    } else if n % 2 == 1 {
        bps[n / 2]
    } else {
        0.5 * (bps[n / 2 - 1] + bps[n / 2])
    };
    let mut cum = 0.0f64;
    let mut peak = 0.0f64;
    let mut dd = 0.0f64;
    for t in trades {
        cum += t.pnl_usd;
        peak = peak.max(cum);
        dd = dd.max(peak - cum);
    }
    let basis: Vec<f64> = trades
        .iter()
        .filter_map(|t| {
            let (e, x) = (t.entry_premium_bps?, t.exit_premium_bps?);
            Some(to_bps(basis_return(t.side, e * 1e-4, x * 1e-4)))
        })
        .collect();
    let count = |r: ExitReason| trades.iter().filter(|t| t.exit_reason == r).count();
    Stats {
        n,
        hit_rate_pnl: trades
            .iter()
            .filter(|t| is_hit(crate::config::HitDefinition::PnlPositive, t))
            .count() as f64
            / nf,
        hit_rate_toward_zero: trades
            .iter()
            .filter(|t| is_hit(crate::config::HitDefinition::PremiumTowardZero, t))
            .count() as f64
            / nf,
        mean_bps: bps.iter().sum::<f64>() / nf,
        median_bps: median,
        mean_basis_bps: basis.iter().sum::<f64>() / basis.len().max(1) as f64,
        total_pnl_usd: trades.iter().map(|t| t.pnl_usd).sum(),
        fees_usd: trades.iter().map(|t| t.fees_usd).sum(),
        funding_usd: trades.iter().map(|t| t.funding_usd).sum(),
        max_drawdown_usd: dd,
        mean_hold_h: trades.iter().map(|t| t.hold_secs()).sum::<f64>() / nf / 3600.0,
        converged_exits: count(ExitReason::Converged),
        max_hold_exits: count(ExitReason::MaxHold),
        kill_exits: count(ExitReason::Kill),
        taker_exits: trades.iter().filter(|t| t.exit_taker_sz > 0.0).count(),
        worst_bps: bps.first().copied().unwrap_or(0.0),
        best_bps: bps.last().copied().unwrap_or(0.0),
    }
}

/// The research reference (results.json, S2_ANT): (n, win_rate_full,
/// mean_full_bps, mean_basis_bps, total_usd_full_at_unit, unit).
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    pub rule: String,
    pub fee_case: String,
    pub n: f64,
    pub win_rate_full: f64,
    pub win_rate_basis: f64,
    pub mean_full_bps: f64,
    pub mean_basis_bps: f64,
    pub total_usd_full_at_unit: f64,
    pub unit_notional_usd: f64,
}

/// Extract the S2_ANT base rule / base fee case from results.json.
pub fn load_reference(path: &Path) -> anyhow::Result<Reference> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_str(&text)?;
    let s = v
        .pointer("/strategies/S2_ANT")
        .ok_or_else(|| anyhow!("no strategies.S2_ANT in {}", path.display()))?;
    let rule = s["base_rule"].as_str().unwrap_or("8h_fixed").to_string();
    let fee_case = s["base_fee_case"]
        .as_str()
        .unwrap_or("growth_maker_maker")
        .to_string();
    let r = s
        .pointer(&format!("/rules/{rule}/fee_cases/{fee_case}"))
        .ok_or_else(|| anyhow!("no rules.{rule}.fee_cases.{fee_case}"))?;
    let f = |k: &str| r[k].as_f64().unwrap_or(f64::NAN);
    Ok(Reference {
        n: f("n"),
        win_rate_full: f("win_rate_full"),
        win_rate_basis: f("win_rate_basis"),
        mean_full_bps: f("mean_full_bps"),
        mean_basis_bps: f("mean_basis_bps"),
        total_usd_full_at_unit: f("total_usd_full_at_unit"),
        unit_notional_usd: v["unit_notional_usd"].as_f64().unwrap_or(10_000.0),
        rule,
        fee_case,
    })
}

/// Human-readable report.
pub fn report(
    cfg: &Config,
    data: &ReplayData,
    res: &ReplayResult,
    refr: Option<&Reference>,
) -> String {
    use std::fmt::Write;
    let st = stats(&res.trades);
    let iso = |ms: u64| {
        let secs = (ms / 1000) as i64;
        let days = secs.div_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:00Z",
            secs.rem_euclid(86_400) / 3600
        )
    };
    let mut o = String::new();
    let _ = writeln!(
        o,
        "replay {}: {} hours, {} -> {}",
        data.coin,
        res.hours,
        iso(res.first_ms),
        iso(res.last_ms)
    );
    let s = &cfg.strategy;
    let _ = writeln!(
        o,
        "config: entry |p| >= {} bps, exit <= {} bps (early exit {}), max hold {} h, notional {} USD, maker {} / taker {}",
        s.entry_threshold_bps,
        s.exit_converge_bps,
        s.early_exit_on_converge,
        s.max_hold_secs / 3600,
        cfg.effective_notional_usd(),
        cfg.fees.maker_rate,
        cfg.fees.taker_rate
    );
    let _ = writeln!(
        o,
        "orders placed {}, post-only rejects {}, entries aborted {}",
        res.orders_placed, res.post_only_rejects, res.entries_aborted
    );
    let _ = writeln!(o, "trades:            {}", st.n);
    let _ = writeln!(o, "hit rate (pnl>0):  {:.1}%", st.hit_rate_pnl * 100.0);
    let _ = writeln!(
        o,
        "hit rate (|p|->0): {:.1}%",
        st.hit_rate_toward_zero * 100.0
    );
    let _ = writeln!(
        o,
        "avg net bps:       {:.1} (median {:.1}, worst {:.1}, best {:.1})",
        st.mean_bps, st.median_bps, st.worst_bps, st.best_bps
    );
    let _ = writeln!(o, "avg basis bps:     {:.1}", st.mean_basis_bps);
    let _ = writeln!(
        o,
        "PnL:               {:.2} USD (fees {:.2}, funding {:.2}), max DD {:.2} USD",
        st.total_pnl_usd, st.fees_usd, st.funding_usd, st.max_drawdown_usd
    );
    let _ = writeln!(
        o,
        "paper account PnL: {:.2} USD (cross-check)",
        res.account_net_pnl_usd
    );
    let _ = writeln!(
        o,
        "avg hold:          {:.2} h; exits: converged {}, max-hold {}, kill {}, with taker part {}",
        st.mean_hold_h, st.converged_exits, st.max_hold_exits, st.kill_exits, st.taker_exits
    );
    for (t, k) in &res.kills {
        let _ = writeln!(o, "KILL at {}: {k}", iso(*t));
    }
    if let Some(r) = refr {
        let unit_scale = cfg.effective_notional_usd() / r.unit_notional_usd;
        let _ = writeln!(
            o,
            "--- research reference (results.json S2_ANT {} / {}) ---",
            r.rule, r.fee_case
        );
        let _ = writeln!(o, "{:<22} {:>12} {:>12}", "", "research", "replay");
        let _ = writeln!(o, "{:<22} {:>12.0} {:>12}", "trades", r.n, st.n);
        let _ = writeln!(
            o,
            "{:<22} {:>11.1}% {:>11.1}%",
            "win rate (full/pnl)",
            r.win_rate_full * 100.0,
            st.hit_rate_pnl * 100.0
        );
        let _ = writeln!(
            o,
            "{:<22} {:>12.1} {:>12.1}",
            "mean net bps (full)", r.mean_full_bps, st.mean_bps
        );
        let _ = writeln!(
            o,
            "{:<22} {:>12.1} {:>12.1}",
            "mean basis bps", r.mean_basis_bps, st.mean_basis_bps
        );
        let _ = writeln!(
            o,
            "{:<22} {:>12.2} {:>12.2}",
            format!("PnL USD @ {:.0}", cfg.effective_notional_usd()),
            r.total_usd_full_at_unit * unit_scale,
            st.total_pnl_usd
        );
    }
    o
}

/// Days since 1970-01-01 -> (year, month, day) (H. Hinnant's algorithm).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Write the trades of a replay as CSV (same columns as the live
/// `trades.csv`, mode `replay`).
pub fn write_trades_csv(path: &Path, trades: &[TradeRecord]) -> anyhow::Result<()> {
    use crate::monitor::csv::{line, TradeRow};
    let mut s = format!("{}\n", TradeRow::HEADER);
    for t in trades {
        s.push_str(&line(&TradeRow { mode: "replay", t }.fields()));
        s.push('\n');
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, s).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::premium::Side;

    fn candle(t: u64, o: f64, h: f64, l: f64, c: f64) -> Candle {
        serde_json::from_value(serde_json::json!({
            "t": t, "T": t + HOUR_MS - 1, "s": "xyz:ANT", "i": "1h",
            "o": o.to_string(), "c": c.to_string(), "h": h.to_string(), "l": l.to_string(),
            "v": "1000", "n": 10
        }))
        .unwrap()
    }

    fn fh(t: u64, p: f64) -> FundingHistoryEntry {
        FundingHistoryEntry {
            coin: "xyz:ANT".into(),
            funding_rate: 0.0,
            premium: p,
            time: t + 7,
        }
    }

    const H0: u64 = 1_800_000_000_000 / HOUR_MS * HOUR_MS;

    #[test]
    fn build_joins_on_px_at_hour() {
        let c = vec![
            candle(H0, 100.0, 101.0, 99.0, 100.5),
            candle(H0 + HOUR_MS, 100.5, 101.0, 100.0, 100.8),
        ];
        let f = vec![
            fh(H0, 0.001),
            fh(H0 + HOUR_MS, 0.002),
            fh(H0 + HOUR_MS + 60_000, 0.9),
        ];
        let d = ReplayData::build("xyz:ANT", &f, &c);
        // H0 has no candle ending at it; H0+1h -> close of the H0 candle.
        assert_eq!(d.bars.len(), 1);
        assert_eq!(d.bars[0].t, H0 + HOUR_MS);
        assert_eq!(d.bars[0].px, 100.5);
        assert_eq!(d.bars[0].premium, 0.002, "first entry per hour wins");
        assert_eq!(d.bars[0].next.as_ref().unwrap().t, H0 + HOUR_MS);
    }

    #[test]
    fn short_entry_fills_on_the_high_and_exits_on_convergence() {
        let cfg = Config::default();
        // Hour 1: premium +100 bps at 100; the hour trades up to 100.5
        // (fills our ask). Hour 2: premium 0 -> converged exit at the bid;
        // hour 2's candle trades down to 99 (fills the exit bid).
        let mut candles = vec![candle(H0, 100.0, 100.0, 100.0, 100.0)];
        candles.push(candle(H0 + HOUR_MS, 100.0, 100.5, 99.9, 100.0));
        candles.push(candle(H0 + 2 * HOUR_MS, 100.0, 100.1, 99.0, 99.5));
        candles.push(candle(H0 + 3 * HOUR_MS, 99.5, 99.6, 99.4, 99.5));
        let f = vec![
            fh(H0 + HOUR_MS, 0.01),
            fh(H0 + 2 * HOUR_MS, 0.0),
            fh(H0 + 3 * HOUR_MS, 0.0),
            fh(H0 + 4 * HOUR_MS, 0.0),
        ];
        let d = ReplayData::build("xyz:ANT", &f, &candles);
        assert_eq!(d.bars.len(), 4);
        let res = run(&cfg, &d, &ReplayParams::default()).unwrap();
        assert_eq!(res.trades.len(), 1, "{res:?}");
        let t = &res.trades[0];
        assert_eq!(t.side, Side::Short);
        assert_eq!(t.exit_reason, ExitReason::Converged);
        assert!(t.entry_px > t.exit_px, "{t:?}");
        assert!(t.pnl_usd > 0.0);
        assert!((res.account_net_pnl_usd - t.pnl_usd).abs() < 1e-6);
        let s = stats(&res.trades);
        assert_eq!(s.n, 1);
        assert_eq!(s.hit_rate_pnl, 1.0);
        assert_eq!(s.converged_exits, 1);
    }

    #[test]
    fn unfilled_entry_is_aborted_without_trades() {
        let cfg = Config::default();
        // Premium signal but the hour never trades up to our ask.
        let candles = vec![
            candle(H0, 100.0, 100.0, 100.0, 100.0),
            candle(H0 + HOUR_MS, 100.0, 100.0, 99.0, 99.5),
        ];
        let f = vec![fh(H0 + HOUR_MS, 0.01), fh(H0 + 2 * HOUR_MS, 0.0)];
        let d = ReplayData::build("xyz:ANT", &f, &candles);
        let res = run(&cfg, &d, &ReplayParams::default()).unwrap();
        assert!(res.trades.is_empty());
        assert!(res.entries_aborted >= 1);
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_769), (2026, 11, 12));
    }
}
