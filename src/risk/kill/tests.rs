//! Unit tests: one block per kill rule, plus stickiness, persistence, reset.

use super::*;
use crate::config::Config;
use crate::strategy::premium::Side;
use crate::strategy::signal::ExitReason;

const T0: u64 = 1_800_000_000_000; // 2027-01-15T08:00:00Z
const S: u64 = 1_000;
const H: u64 = 3_600 * S;

fn mgr() -> RiskManager {
    RiskManager::new(
        RiskParams::from(&Config::default().risk),
        RiskState::default(),
    )
}

fn trade(entry_bps: f64, exit_bps: f64, pnl: f64) -> TradeRecord {
    TradeRecord {
        side: Side::Short,
        size: 10.0,
        entry_px: 100.0,
        exit_px: 100.0,
        signal_ms: T0,
        opened_ms: T0,
        closed_ms: T0 + H,
        entry_premium_bps: Some(entry_bps),
        exit_premium_bps: Some(exit_bps),
        fees_usd: 0.0,
        funding_usd: 0.0,
        gross_pnl_usd: pnl,
        pnl_usd: pnl,
        pnl_bps: pnl / 1000.0 * 1e4,
        exit_reason: ExitReason::MaxHold,
        exit_taker_sz: 0.0,
    }
}

fn hit() -> TradeRecord {
    trade(60.0, 10.0, 5.0)
}

fn miss() -> TradeRecord {
    trade(60.0, 70.0, -5.0)
}

fn live_spec() -> AssetSpec {
    let pd: Vec<Option<crate::hl::types::PerpDex>> =
        serde_json::from_str(include_str!("../../../tests/fixtures/perpDexs.json")).unwrap();
    let meta: crate::hl::types::Meta =
        serde_json::from_str(include_str!("../../../tests/fixtures/meta_xyz.json")).unwrap();
    crate::hl::assets::resolve_asset(&pd, &meta, "xyz", "xyz:ANT").unwrap()
}

// ------------------------------------------------------------- hit rate

#[test]
fn hit_rate_kills_on_fifth_miss_in_window_of_20() {
    // window 20, min 0.80 -> at most 4 misses
    let mut m = mgr();
    for _ in 0..16 {
        assert!(m.on_trade_closed(T0, &hit()).is_none());
    }
    for _ in 0..4 {
        assert!(m.on_trade_closed(T0, &miss()).is_none());
    }
    assert_eq!(m.hit_rate(), Some((16, 20))); // exactly 80 %: allowed
    let k = m.on_trade_closed(T0 + S, &miss()).expect("kill");
    assert!(matches!(
        k.reason,
        KillReason::HitRate {
            hits: 15,
            trades: 20,
            ..
        }
    ));
    assert!(k.cancel_orders && k.stop_entries && k.flatten);
    assert!(k.alert.contains("hit_rate"));
    assert!(!m.entries_allowed());
}

#[test]
fn hit_rate_rolls_old_misses_out() {
    let mut m = mgr();
    for _ in 0..4 {
        m.on_trade_closed(T0, &miss());
    }
    for _ in 0..20 {
        assert!(m.on_trade_closed(T0, &hit()).is_none());
    }
    assert_eq!(m.hit_rate(), Some((20, 20)));
    for _ in 0..4 {
        assert!(m.on_trade_closed(T0, &miss()).is_none());
    }
    assert!(m.on_trade_closed(T0, &miss()).is_some());
}

#[test]
fn hit_rate_fires_early_once_certain() {
    // 5 misses in the first 5 trades: no 20-trade window starting here can
    // reach 80 % any more.
    let mut m = mgr();
    for i in 0..4 {
        assert!(m.on_trade_closed(T0, &miss()).is_none(), "miss {i}");
    }
    assert!(m.on_trade_closed(T0, &miss()).is_some());
}

#[test]
fn hit_definitions() {
    let t = trade(60.0, 20.0, -1.0);
    assert!(is_hit(HitDefinition::PremiumTowardZero, &t));
    assert!(!is_hit(HitDefinition::PnlPositive, &t));
    let t = trade(-60.0, -70.0, 3.0);
    assert!(!is_hit(HitDefinition::PremiumTowardZero, &t));
    assert!(is_hit(HitDefinition::PnlPositive, &t));
    let mut t = hit();
    t.exit_premium_bps = None;
    assert!(!is_hit(HitDefinition::PremiumTowardZero, &t));
}

// ------------------------------------------------------------- persistent premium

#[test]
fn persistent_premium_kills_after_duration() {
    let mut m = mgr(); // 150 bps for 7200 s
    assert!(m.on_premium(T0, Some(160.0)).is_none());
    assert!(
        m.on_premium(T0 + H, Some(-170.0)).is_none(),
        "|premium| counts"
    );
    assert!(
        m.on_premium(T0 + H + S, None).is_none(),
        "gap keeps the timer"
    );
    assert!(m.on_premium(T0 + 2 * H - 1, Some(200.0)).is_none());
    let k = m.on_premium(T0 + 2 * H, Some(155.0)).expect("kill");
    match k.reason {
        KillReason::PersistentPremium {
            since_ms,
            duration_secs,
            ..
        } => {
            assert_eq!(since_ms, T0);
            assert_eq!(duration_secs, 7200);
        }
        r => panic!("{r:?}"),
    }
}

#[test]
fn persistent_premium_resets_below_threshold() {
    let mut m = mgr();
    m.on_premium(T0, Some(160.0));
    assert!(
        m.on_premium(T0 + H, Some(150.0)).is_none(),
        "at threshold resets"
    );
    assert_eq!(m.state().premium_above_since_ms, None);
    assert!(m.on_premium(T0 + 2 * H, Some(160.0)).is_none());
    assert!(m.on_premium(T0 + 3 * H, Some(160.0)).is_none());
    assert!(m.on_premium(T0 + 4 * H, Some(160.0)).is_some());
    assert!(m.on_premium(T0, Some(f64::NAN)).is_none());
}

// ------------------------------------------------------------- spec change

#[test]
fn spec_first_snapshot_becomes_baseline() {
    let mut m = mgr();
    let c = m.on_spec(T0, &live_spec(), &Config::default().market);
    assert!(c.changes.is_empty(), "{:?}", c.changes);
    assert!(c.kill.is_none());
    assert_eq!(m.state().baseline_spec.as_ref().unwrap().asset_id, 110_138);
    // unchanged on the next poll
    assert!(m
        .on_spec(T0 + S, &live_spec(), &Config::default().market)
        .changes
        .is_empty());
}

#[test]
fn spec_baseline_mismatch_with_config_kills() {
    let mut m = mgr();
    let mut cfg = Config::default().market;
    cfg.expected_funding_multiplier = 0.01;
    let c = m.on_spec(T0, &live_spec(), &cfg);
    assert_eq!(c.changes.len(), 1);
    assert!(matches!(
        c.kill.unwrap().reason,
        KillReason::SpecChange { .. }
    ));
}

#[test]
fn spec_change_kills_for_each_watched_field() {
    type Mutator = fn(&mut AssetSpec);
    let cases: [(&str, Mutator); 8] = [
        ("funding_multiplier", |a| a.funding_multiplier = Some(0.05)),
        ("oracle_updater", |a| a.oracle_updater = Some("0x1".into())),
        ("halted", |a| a.is_delisted = true),
        ("growth_mode", |a| a.growth_mode = Some("disabled".into())),
        ("deployer_fee_scale", |a| a.deployer_fee_scale = Some(2.0)),
        ("sz_decimals", |a| a.sz_decimals = 3),
        ("max_leverage", |a| a.max_leverage = 3),
        ("margin_mode", |a| a.margin_mode = Some("noCross".into())),
    ];
    for (name, f) in cases {
        let mut m = mgr();
        m.on_spec(T0, &live_spec(), &Config::default().market);
        let mut changed = live_spec();
        f(&mut changed);
        let c = m.on_spec(T0 + S, &changed, &Config::default().market);
        assert!(
            c.changes.iter().any(|x| x.starts_with(name)),
            "{name}: {:?}",
            c.changes
        );
        let k = c.kill.unwrap_or_else(|| panic!("{name}: no kill"));
        assert!(k.alert.contains(name));
        assert!(m.is_killed());
    }
}

#[test]
fn spec_change_without_kill_only_reports_once() {
    let mut p = RiskParams::from(&Config::default().risk);
    p.kill_on_spec_change = false;
    let mut m = RiskManager::new(p, RiskState::default());
    m.on_spec(T0, &live_spec(), &Config::default().market);
    let mut changed = live_spec();
    changed.growth_mode = Some("disabled".into());
    let c = m.on_spec(T0 + S, &changed, &Config::default().market);
    assert_eq!(c.changes.len(), 1);
    assert!(c.kill.is_none());
    assert!(!m.is_killed());
    assert!(m
        .on_spec(T0 + 2 * S, &changed, &Config::default().market)
        .changes
        .is_empty());
}

#[test]
fn spec_unavailable_kills() {
    let mut m = mgr();
    let c = m.on_spec_unavailable(T0, "xyz:ANT not in meta");
    assert!(c.kill.is_some());
}

// ------------------------------------------------------------- daily loss

#[test]
fn daily_loss_limit_utc_day() {
    let mut m = mgr(); // 150 USD
    assert!(m.on_equity(T0, 1_000.0).is_none());
    assert!(m.on_equity(T0 + H, 900.0).is_none());
    assert_eq!(m.daily_pnl(), Some(-100.0));
    // gains earlier in the day do not lower the start-of-day reference
    let k = m.on_equity(T0 + 2 * H, 850.0).expect("kill at -150");
    match k.reason {
        KillReason::DailyLoss {
            utc_day, pnl_usd, ..
        } => {
            assert_eq!(utc_day, utc_day_of(T0));
            assert_eq!(pnl_usd, -150.0);
        }
        r => panic!("{r:?}"),
    }
}

fn utc_day_of(t: u64) -> u64 {
    utc_day(t)
}

#[test]
fn daily_loss_resets_on_new_utc_day() {
    let mut p = RiskParams::from(&Config::default().risk);
    p.max_drawdown_usd = 10_000.0;
    let mut m = RiskManager::new(p, RiskState::default());
    let day_end = (utc_day(T0) + 1) * 86_400_000 - 1;
    assert!(m.on_equity(day_end - H, 1_000.0).is_none());
    assert!(m.on_equity(day_end, 870.0).is_none()); // -130 on day 1
                                                    // next UTC day: start = 870 (last equity of the previous day)
    assert!(m.on_equity(day_end + 1, 860.0).is_none());
    assert_eq!(m.daily_pnl(), Some(-10.0));
    assert!(m.on_equity(day_end + H, 730.0).is_none()); // -140
    assert!(m.on_equity(day_end + 2 * H, 720.0).is_some()); // -150
}

// ------------------------------------------------------------- drawdown

#[test]
fn max_drawdown_from_peak() {
    let mut p = RiskParams::from(&Config::default().risk);
    p.daily_loss_limit_usd = 10_000.0;
    let mut m = RiskManager::new(p, RiskState::default()); // 300 USD
    m.on_equity(T0, 0.0);
    m.on_equity(T0 + S, 400.0);
    assert!(m.on_equity(T0 + 2 * S, 101.0).is_none());
    assert_eq!(m.drawdown(), Some(299.0));
    let k = m.on_equity(T0 + 3 * S, 100.0).expect("kill");
    assert!(matches!(
        k.reason,
        KillReason::MaxDrawdown { peak_usd, drawdown_usd, .. } if peak_usd == 400.0 && drawdown_usd == 300.0
    ));
    assert!(m.on_equity(T0, f64::NAN).is_none());
}

// ------------------------------------------------------------- kill semantics

#[test]
fn kill_is_sticky_and_records_later_triggers() {
    let mut m = mgr();
    let k = m.trigger_manual(T0, "operator").unwrap();
    assert_eq!(k.reason.rule(), "manual");
    assert!(m.trigger_manual(T0 + S, "again").is_none());
    for _ in 0..5 {
        assert!(m.on_trade_closed(T0 + 2 * S, &miss()).is_none());
    }
    let rec = m.kill_record().unwrap();
    assert_eq!(rec.at_ms, T0);
    assert_eq!(rec.later.len(), 2);
    assert!(!m.entries_allowed());
}

#[test]
fn flatten_follows_config() {
    let mut p = RiskParams::from(&Config::default().risk);
    p.flatten_on_kill = false;
    let mut m = RiskManager::new(p, RiskState::default());
    let k = m.trigger_manual(T0, "x").unwrap();
    assert!(!k.flatten && k.cancel_orders && k.stop_entries);
}

#[test]
fn persisted_kill_survives_restart_until_manual_reset() {
    let dir = tempfile::tempdir().unwrap();
    let path = risk_state_path(dir.path());
    assert_eq!(RiskState::load(&path).unwrap(), None);

    let mut m = mgr();
    m.on_equity(T0, 0.0);
    m.on_equity(T0 + S, -300.0);
    assert!(m.is_killed());
    m.state().save(&path).unwrap();

    // restart: still killed
    let st = RiskState::load(&path).unwrap().unwrap();
    let mut m2 = RiskManager::new(RiskParams::from(&Config::default().risk), st);
    assert!(m2.is_killed());
    assert!(!m2.entries_allowed());
    assert_eq!(m2.kill_record().unwrap().reason.rule(), "daily_loss");

    // manual reset clears the kill and the conditions that would re-fire
    let cleared = m2.reset().unwrap();
    assert_eq!(cleared.reason.rule(), "daily_loss");
    assert!(m2.entries_allowed());
    assert!(
        m2.on_equity(T0 + 2 * S, -300.0).is_none(),
        "fresh day start / peak"
    );
    m2.state().save(&path).unwrap();
    assert!(RiskState::load(&path).unwrap().unwrap().kill.is_none());
}

#[test]
fn reset_state_helper_and_corrupt_file() {
    let mut m = mgr();
    for _ in 0..5 {
        m.on_trade_closed(T0, &miss());
    }
    let mut st = m.state().clone();
    assert!(reset_state(&mut st).is_some());
    assert!(st.outcomes.is_empty() && st.kill.is_none());
    assert!(reset_state(&mut st).is_none());

    let dir = tempfile::tempdir().unwrap();
    let path = risk_state_path(dir.path());
    std::fs::write(&path, "{not json").unwrap();
    assert!(RiskState::load(&path).is_err(), "never silently un-kill");
}

#[test]
fn smaller_window_in_new_config_truncates_history() {
    let st = RiskState {
        outcomes: std::iter::repeat_n(false, 3)
            .chain(std::iter::repeat_n(true, 10))
            .collect(),
        ..Default::default()
    };
    let mut p = RiskParams::from(&Config::default().risk);
    p.hit_rate_window = 10;
    let m = RiskManager::new(p, st);
    assert_eq!(m.hit_rate(), Some((10, 10)));
}

#[test]
fn describe_is_english_and_informative() {
    let r = KillReason::SpecChange {
        changes: vec!["growth_mode: a -> b".into()],
    };
    assert!(r.describe().contains("market spec changed"));
    let r = KillReason::HitRate {
        hits: 15,
        trades: 20,
        window: 20,
        min_rate: 0.8,
    };
    assert!(r.describe().contains("15/20"));
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains("\"rule\":\"hit_rate\""));
}

#[test]
fn trades_with_unknown_premium_are_not_hit_rate_misses() {
    let mut r = RiskManager::new(
        RiskParams::from(&Config::default().risk),
        RiskState::default(),
    );
    let mut adopted = miss();
    adopted.entry_premium_bps = None;
    let mut external = miss();
    external.exit_premium_bps = None;
    for _ in 0..10 {
        assert!(r.on_trade_closed(T0, &adopted).is_none());
        assert!(r.on_trade_closed(T0, &external).is_none());
    }
    assert!(r.state().outcomes.is_empty());
    assert_eq!(r.state().trades_closed, 20);
    assert!(!r.is_killed());
    // With the PnL definition every trade counts.
    let mut p = RiskParams::from(&Config::default().risk);
    p.hit_definition = HitDefinition::PnlPositive;
    let mut r = RiskManager::new(p, RiskState::default());
    r.on_trade_closed(T0, &adopted);
    assert_eq!(r.state().outcomes.len(), 1);
}
