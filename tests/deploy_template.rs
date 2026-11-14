//! The Docker strategy template (`deploy/strategy/`) stays in sync with the
//! config schema and keeps its safety defaults. Read-only, no network.

use std::path::{Path, PathBuf};

use antbot::config::{Config, Mode};

fn template(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("deploy/strategy")
        .join(file)
}

fn read(file: &str) -> String {
    std::fs::read_to_string(template(file)).unwrap()
}

#[test]
fn config_example_loads_validates_and_is_paper_in_data() {
    // Config::load rejects unknown keys and runs every validation.
    let cfg = Config::load(template("config.example.toml")).unwrap();
    assert_eq!(cfg.mode, Mode::Paper);
    assert_eq!(cfg.monitor.data_dir, PathBuf::from("/data"));
    // A share of the 20 weight/s per-IP budget, not all of it.
    assert!(cfg.timing.rest_refill_per_sec > 0.0 && cfg.timing.rest_refill_per_sec <= 10.0);
    // Unset keys come from config/default.toml (Config::default mirrors it).
    let d = Config::default();
    assert_eq!(cfg.strategy, d.strategy);
    assert_eq!(cfg.network, d.network);
    // new-strategy.sh rewrites exactly this line for --mode.
    let text = read("config.example.toml");
    assert_eq!(text.lines().filter(|l| *l == "mode = \"paper\"").count(), 1);
}

#[test]
fn compose_and_env_templates_keep_their_guards() {
    let compose = read("compose.yaml");
    assert!(
        !compose
            .lines()
            .any(|l| l.trim_start().starts_with("ports:")),
        "no published port"
    );
    for needle in [
        "read_only: true",
        "cap_drop: [ALL]",
        "no-new-privileges:true",
        "user: \"10001:10001\"",
        "ANTBOT_HEALTH_LISTEN: 127.0.0.1:9100",
        "command: run ${ANTBOT_MAINNET_FLAG:-}",
        "profiles: [testnet]",
    ] {
        assert!(compose.contains(needle), "compose.yaml lost {needle:?}");
    }
    assert!(!compose.contains("env_file"), "explicit environment only");

    // .env never enables mainnet nor carries a secret value.
    let env = read(".env.example");
    for l in env.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        assert!(
            k != "ANTBOT_MAINNET_FLAG" && k != "ANTBOT_CONFIRM_MAINNET",
            "{l}"
        );
        for secret in ["HL_PRIVATE_KEY", "HL_ACCOUNT_ADDRESS", "TELEGRAM_BOT_TOKEN"] {
            assert_ne!(k, secret, "secrets go to files, not .env");
        }
        if k.ends_with("_FILE") {
            assert!(v.starts_with("/run/secrets/"), "{l}");
        }
    }
}
