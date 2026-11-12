//! Drive the pure strategy + risk state machines with the research backtest's
//! hourly premium series (fundingHistory xyz:ANT) and compare them with
//! `hl-research/sim/trades.csv` (S2_ANT, rule 8h_fixed, 24 trades).
//!
//! Fixtures (copied from the research data, see their headers):
//! * `backtest_ant_premium_1h.json`: hour-floored fundingHistory premium,
//!   first entry per hour, exactly as `backtest.py load()`.
//! * `backtest_ant_8h_trades.csv`: the 24 backtest trades.
//!
//! Fills are instant at the hour's price, so this checks the signal / hold
//! / cooldown logic and the trade arithmetic, not the fill model.

use std::collections::HashMap;

use antbot::config::{Config, HitDefinition};
use antbot::risk::kill::{KillReason, RiskManager, RiskParams, RiskState};
use antbot::strategy::premium::{basis_return, to_bps, Side};
use antbot::strategy::signal::{
    Command, DoneReason, ExitReason, MarketView, OrderKind, OrderUpdate, Strategy, StrategyParams,
    TickInput, TradeRecord,
};

const HOUR: u64 = 3_600_000;

struct BtTrade {
    entry_ms: u64,
    exit_ms: u64,
    side: Side,
    p_entry: f64,
    p_exit: f64,
    px_entry: f64,
    px_exit: f64,
    basis_bps: f64,
    full_bps: f64,
}

fn premiums() -> Vec<(u64, f64)> {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/backtest_ant_premium_1h.json")).unwrap();
    v["hourly_premium"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_u64().unwrap(), r[1].as_f64().unwrap()))
        .collect()
}

fn bt_trades() -> Vec<BtTrade> {
    let text = include_str!("fixtures/backtest_ant_8h_trades.csv");
    let mut lines = text.lines();
    let hdr: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |name: &str| hdr.iter().position(|h| *h == name).unwrap();
    lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            let num = |n: &str| f[col(n)].parse::<f64>().unwrap();
            BtTrade {
                entry_ms: f[col("entry_ms")].parse().unwrap(),
                exit_ms: f[col("exit_ms")].parse().unwrap(),
                side: if f[col("side")] == "long" {
                    Side::Long
                } else {
                    Side::Short
                },
                p_entry: num("p_entry"),
                p_exit: num("p_exit"),
                px_entry: num("px_entry"),
                px_exit: num("px_exit"),
                basis_bps: num("basis_bps"),
                full_bps: num("full_bps"),
            }
        })
        .collect()
}

/// Replay hourly ticks with instant full fills at the hour's price.
fn replay(params: StrategyParams, px_at: &HashMap<u64, f64>) -> Vec<TradeRecord> {
    let mut s = Strategy::new(params);
    let mut out = Vec::new();
    for (h, p) in premiums() {
        let px = *px_at.get(&h).unwrap_or(&100.0);
        let view = MarketView {
            now_ms: h,
            premium_bps: Some(to_bps(p)),
            best_bid: Some(px),
            best_ask: Some(px),
            stale: false,
        };
        let mut queue = s
            .on_tick(TickInput {
                view,
                entries_allowed: true,
                available_margin_usd: None,
            })
            .commands;
        while let Some(cmd) = queue.pop() {
            let Command::Place(o) = cmd else { continue };
            let fee = px
                * o.sz
                * if o.kind == OrderKind::Ioc {
                    0.00009
                } else {
                    0.00003
                };
            for upd in [
                OrderUpdate::Filled {
                    tag: Some(o.tag),
                    is_buy: o.is_buy,
                    px,
                    sz: o.sz,
                    fee_usd: fee,
                    crossed: o.kind == OrderKind::Ioc,
                    time_ms: h,
                },
                OrderUpdate::Done {
                    tag: o.tag,
                    reason: DoneReason::Filled,
                },
            ] {
                let st = s.on_order_update(h, upd);
                for e in st.events {
                    if let antbot::strategy::signal::Event::TradeClosed(r) = e {
                        out.push(r);
                    }
                }
                queue.extend(st.commands);
            }
        }
    }
    out
}

fn px_map(bt: &[BtTrade]) -> HashMap<u64, f64> {
    let mut m = HashMap::new();
    for t in bt {
        m.insert(t.entry_ms, t.px_entry);
        m.insert(t.exit_ms, t.px_exit);
    }
    m
}

fn backtest_params() -> StrategyParams {
    let mut cfg = Config::default();
    cfg.strategy.early_exit_on_converge = false; // base rule 8h_fixed
    cfg.validate().unwrap();
    StrategyParams::from_config(&cfg, 2, 10)
}

#[test]
fn reproduces_the_24_backtest_trades() {
    let bt = bt_trades();
    assert_eq!(bt.len(), 24);
    let got = replay(backtest_params(), &px_map(&bt));
    assert_eq!(got.len(), bt.len(), "trade count");
    for (r, b) in got.iter().zip(&bt) {
        assert_eq!(r.opened_ms, b.entry_ms, "entry hour");
        assert_eq!(r.closed_ms, b.exit_ms, "exit hour (8h)");
        assert_eq!(r.side, b.side);
        assert_eq!(r.exit_reason, ExitReason::MaxHold);
        // premiums recorded at entry / exit are the backtest's p_entry / p_exit
        assert!((r.entry_premium_bps.unwrap() - to_bps(b.p_entry)).abs() < 1e-9);
        assert!((r.exit_premium_bps.unwrap() - to_bps(b.p_exit)).abs() < 1e-9);
        // basis arithmetic
        let basis = to_bps(basis_return(
            r.side,
            r.entry_premium_bps.unwrap() * 1e-4,
            r.exit_premium_bps.unwrap() * 1e-4,
        ));
        assert!(
            (basis - b.basis_bps).abs() < 1e-6,
            "{basis} vs {}",
            b.basis_bps
        );
        // full mark-to-mark return = gross PnL / entry notional
        let full = r.gross_pnl_usd / (r.entry_px * r.size) * 1e4;
        assert!((full - b.full_bps).abs() < 1e-6, "{full} vs {}", b.full_bps);
        // fees: maker entry + taker exit, net = gross - fees
        assert!((r.pnl_usd - (r.gross_pnl_usd - r.fees_usd)).abs() < 1e-9);
        assert!(r.size * r.entry_px <= 2_500.0 + 1e-9);
    }
}

#[test]
fn risk_rules_on_backtest_history() {
    let bt = bt_trades();
    let trades = replay(backtest_params(), &px_map(&bt));

    // Writeup definition ("toward 0"): 23/24 hits -> never killed.
    let mut m = RiskManager::new(
        RiskParams::from(&Config::default().risk),
        RiskState::default(),
    );
    for t in &trades {
        assert!(m.on_trade_closed(t.closed_ms, t).is_none());
    }
    assert_eq!(m.hit_rate(), Some((19, 20)));

    // PnL definition: 18/24 winners -> the 5th loser in the window kills.
    let mut p = RiskParams::from(&Config::default().risk);
    p.hit_definition = HitDefinition::PnlPositive;
    let mut m = RiskManager::new(p, RiskState::default());
    let killed_at = trades
        .iter()
        .position(|t| m.on_trade_closed(t.closed_ms, t).is_some());
    assert!(killed_at.is_some());
    assert!(matches!(
        m.kill_record().unwrap().reason,
        KillReason::HitRate { .. }
    ));

    // Persistent premium (> 150 bps for 2h): the longest hourly run above
    // 150 bps spans 1h (launch hours), so the rule never fires on history.
    let mut m = RiskManager::new(
        RiskParams::from(&Config::default().risk),
        RiskState::default(),
    );
    for (h, p) in premiums() {
        assert!(m.on_premium(h, Some(to_bps(p))).is_none(), "fired at {h}");
    }
    // ...but a 1h requirement would have fired at launch.
    let mut p = RiskParams::from(&Config::default().risk);
    p.persistent_premium_ms = HOUR;
    let mut m = RiskManager::new(p, RiskState::default());
    let first = premiums()
        .into_iter()
        .find_map(|(h, p)| m.on_premium(h, Some(to_bps(p))).map(|_| h));
    assert_eq!(first, Some(premiums()[0].0 + HOUR));
}

#[test]
fn early_exit_variant_is_consistent() {
    // Default config (early exit at 10 bps): more, shorter trades; invariants hold.
    let params = StrategyParams::from_config(&Config::default(), 2, 10);
    let trades = replay(params, &HashMap::new());
    assert!(trades.len() >= 24, "{}", trades.len());
    let mut last_close = 0;
    for t in &trades {
        assert!(t.opened_ms >= last_close + HOUR, "cooldown after exit");
        assert!(t.closed_ms - t.opened_ms <= 8 * HOUR, "8h max hold");
        assert!(t.entry_premium_bps.unwrap().abs() >= 50.0);
        if t.exit_reason == ExitReason::Converged {
            let e = t.entry_premium_bps.unwrap();
            let x = t.exit_premium_bps.unwrap();
            assert!(e.signum() * x <= 10.0, "converged exit {e} -> {x}");
        }
        last_close = t.closed_ms;
    }
}

#[test]
fn kill_order_drives_strategy_cancel_and_flatten() {
    // risk -> strategy wiring: a drawdown kill cancels the resting exit and
    // flattens, and the persisted kill blocks entries afterwards.
    let mut s = Strategy::new(StrategyParams::from_config(&Config::default(), 2, 10));
    let mut risk = RiskManager::new(
        RiskParams::from(&Config::default().risk),
        RiskState::default(),
    );
    let t0 = 1_800_000_000_000u64;
    let view = |now, prem: f64| TickInput {
        view: MarketView {
            now_ms: now,
            premium_bps: Some(prem),
            best_bid: Some(100.0),
            best_ask: Some(100.02),
            stale: false,
        },
        entries_allowed: true,
        available_margin_usd: None,
    };
    let st = s.on_tick(view(t0, 60.0));
    let Command::Place(o) = st.commands[0] else {
        panic!()
    };
    s.on_order_update(
        t0,
        OrderUpdate::Filled {
            tag: Some(o.tag),
            is_buy: o.is_buy,
            px: o.px,
            sz: o.sz,
            fee_usd: 0.0,
            crossed: false,
            time_ms: t0,
        },
    );
    s.on_order_update(
        t0,
        OrderUpdate::Done {
            tag: o.tag,
            reason: DoneReason::Filled,
        },
    );
    risk.on_equity(t0, 0.0);
    let k = risk.on_equity(t0 + 1_000, -300.0).expect("drawdown kill");
    let st = s.on_kill(t0 + 1_000, k.flatten, &k.alert);
    assert!(k.cancel_orders && k.flatten && k.stop_entries);
    let Command::Place(x) = st.commands[0] else {
        panic!("{st:?}")
    };
    assert!(x.reduce_only && x.is_buy);
    assert!(!risk.entries_allowed());
}
