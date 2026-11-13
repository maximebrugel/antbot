//! Regression: state files written by earlier releases still load.
//!
//! `tests/fixtures/persisted/*_v1.json` were produced by the code before
//! the multi-strategy refactor (a mainnet bot after one closed trade, with
//! a partially filled entry: position + working order, live executor
//! orders, premium samples, day tracker; and a killed risk state with a
//! spec baseline). They must deserialize into today's types and serialize
//! back to exactly the same JSON, so a running deployment can be upgraded
//! in place.

use std::path::{Path, PathBuf};

use antbot::config::{Config, Mode};
use antbot::engine::{Engine, EngineParams};
use antbot::exec::live::LiveState;
use antbot::exec::state::{BotState, STATE_VERSION};
use antbot::risk::{RiskManager, RiskParams, RiskState};
use antbot::strategy::signal::{Phase, StrategyParams, StrategyState};
use antbot::strategy::{Strategy, TradingStrategy};
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/persisted")
        .join(name)
}

fn json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn state_mainnet_v1_loads_and_roundtrips_unchanged() {
    let path = fixture("state_mainnet_v1.json");
    let st = BotState::load(&path).unwrap().expect("fixture present");
    assert_eq!(st.version, STATE_VERSION);
    assert_eq!(st.mode, Mode::Mainnet);
    assert_eq!(st.coin, "xyz:ANT");
    assert!(matches!(st.strategy.phase, Phase::Entering { .. }));
    let pos = st.strategy.position.as_ref().unwrap();
    assert_eq!(pos.size, 5.0);
    assert_eq!(st.strategy.working.as_ref().unwrap().intent.tag, 3);
    assert_eq!(st.ledger.trades.len(), 1);
    assert_eq!(st.premium_samples.len(), 2);
    assert_eq!(st.day_tracker.as_ref().unwrap().day, Some(20_833));
    // Live executor state inside.
    let live: LiveState = serde_json::from_value(st.executor.clone()).unwrap();
    assert_eq!(live.orders.len(), 2);
    assert_eq!(live.orders[1].oid, Some(4343));
    // Same JSON back (no field renamed, added or dropped).
    assert_eq!(serde_json::to_value(&st).unwrap(), json(&path));
    assert_eq!(serde_json::to_value(&live).unwrap(), st.executor);
    // The generic loader reads the same file.
    let g = BotState::<StrategyState>::load_as(&path).unwrap().unwrap();
    assert_eq!(g, st);
    // Save / load through a temp dir keeps it identical.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("state_mainnet.json");
    st.save(&p).unwrap();
    assert_eq!(json(&p), json(&path));
}

#[test]
fn risk_state_v1_loads_and_roundtrips_unchanged() {
    let path = fixture("risk_state_v1.json");
    let rs = RiskState::load(&path).unwrap().expect("fixture present");
    let k = rs.kill.as_ref().unwrap();
    assert_eq!(k.reason.rule(), "manual");
    assert_eq!(k.later.len(), 1);
    assert_eq!(rs.trades_closed, 1);
    let base = rs.baseline_spec.as_ref().unwrap();
    assert_eq!(base.asset_id, 110_138);
    assert_eq!(base.funding_multiplier, Some(0.005));
    assert_eq!(serde_json::to_value(&rs).unwrap(), json(&path));
}

#[test]
fn v1_state_resumes_into_the_generic_engine() {
    let cfg = Config::default();
    let st = BotState::load(&fixture("state_mainnet_v1.json"))
        .unwrap()
        .unwrap();
    let rs = RiskState::load(&fixture("risk_state_v1.json"))
        .unwrap()
        .unwrap();
    let mut e: Engine<Strategy> = Engine::new(
        EngineParams::from_config(&cfg),
        Strategy::from_state(
            StrategyParams::from_config(&cfg, 2, 10),
            st.strategy.clone(),
        ),
        RiskManager::new(RiskParams::from(&cfg.risk), rs),
        st.ledger.clone(),
        cfg.strategy.premium_avg_secs * 1000,
    );
    assert_eq!(TradingStrategy::state(&e.strategy), &st.strategy);
    assert_eq!(e.strategy_position(), 5.0);
    // The persisted kill is honoured at startup: the working entry is
    // canceled (the strategy then flattens the 5.0 long, flatten_on_kill).
    let cmds = e.startup(1_800_003_700_000);
    assert!(!cmds.is_empty(), "the working entry is canceled");
    assert!(e.strategy.state().kill_reason.is_some());
}

#[test]
fn older_minimal_state_shapes_still_load() {
    // A v1 file written before the optional fields existed (no executor,
    // premium_samples, day_tracker): serde defaults fill them in.
    let mut v = json(&fixture("state_mainnet_v1.json"));
    let o = v.as_object_mut().unwrap();
    for k in ["executor", "premium_samples", "day_tracker"] {
        o.remove(k);
    }
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("state_mainnet.json");
    std::fs::write(&p, serde_json::to_string(&v).unwrap()).unwrap();
    let st = BotState::load(&p).unwrap().unwrap();
    assert!(st.executor.is_null());
    assert!(st.premium_samples.is_empty());
    assert!(st.day_tracker.is_none());
    // Risk state without the later-trigger list.
    let mut r = json(&fixture("risk_state_v1.json"));
    r["kill"].as_object_mut().unwrap().remove("later");
    let p = dir.path().join("risk_state.json");
    std::fs::write(&p, serde_json::to_string(&r).unwrap()).unwrap();
    assert!(RiskState::load(&p)
        .unwrap()
        .unwrap()
        .kill
        .unwrap()
        .later
        .is_empty());
}

#[test]
fn shipped_default_config_still_loads() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(antbot::config::DEFAULT_CONFIG_PATH);
    assert_eq!(Config::load(&p).unwrap(), Config::default());
    // A config written by the previous release (log_max_files = 30, no new
    // keys) is still valid as is.
    let old = std::fs::read_to_string(&p)
        .unwrap()
        .replace("log_max_files = 7 ", "log_max_files = 30");
    let c = Config::from_toml_str(&old).unwrap();
    assert_eq!(c.monitor.log_max_files, 30);
    assert_eq!(c.live.vault_address, None);
    assert_eq!(c.monitor.health_listen, None);
}
