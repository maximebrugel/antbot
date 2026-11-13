//! The private key never appears in formatted output: errors (Display,
//! Debug, anyhow chains), Debug of every type that holds it, and the config
//! error path (scrubbed). Dummy keys only; nothing leaves the machine.

use std::time::Duration;

use antbot::config::{Config, Mode};
use antbot::hl::exchange::{build_for_mode, ExchangeClient};
use antbot::mode::{self, Credentials, PrivateKey};

const KEY: &str = "0x8f2a5949ab7a7ed1c1e1a4c47d1e4b0d7a3ef0c2b1a9e8d7c6b5a4f3e2d1c0b9";

fn assert_no_key(s: &str) {
    let hex = &KEY[2..];
    let l = s.to_ascii_lowercase();
    assert!(!l.contains(hex), "key leaked in: {s}");
    // Not even a long fragment of it.
    for w in hex.as_bytes().windows(16) {
        assert!(
            !l.contains(std::str::from_utf8(w).unwrap()),
            "key fragment leaked in: {s}"
        );
    }
}

fn creds(key: &str, account: Option<&str>) -> Credentials {
    Credentials {
        private_key: PrivateKey::parse(key).unwrap(),
        account_address: account.map(str::to_string),
        vault_address: None,
    }
}

#[test]
fn credentials_and_client_debug_are_redacted() {
    let c = creds(KEY, Some("0x00000000000000000000000000000000000000aa"));
    assert_no_key(&format!("{c:?}"));
    assert_no_key(&format!("{c:#?}"));
    assert_no_key(&format!("{:?}", c.private_key));
    let w = c.private_key.wallet().unwrap();
    assert_no_key(&format!("{w:?}"));
    let cfg = Config::default();
    let client = ExchangeClient::new(
        Mode::Testnet,
        &cfg.endpoints(Mode::Testnet),
        &c,
        None,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_no_key(&format!("{client:?}"));
    assert_no_key(&format!("{client:#?}"));
}

#[test]
fn exchange_errors_never_contain_the_key() {
    let cfg = Config::default();
    // Right format, invalid scalar (above the curve order).
    let bad = format!("0x{}", "f".repeat(64));
    let e = build_for_mode(
        &cfg,
        Mode::Testnet,
        &creds(&bad, None),
        None,
        std::sync::Arc::new(antbot::hl::info::TokenBucket::new(10.0, 1.0)),
    )
    .unwrap_err();
    let s = format!("{e:#}");
    assert!(s.contains("malformed private key"), "{s}");
    assert!(!s.contains(&"f".repeat(64)), "{s}");
    assert!(!format!("{e:?}").contains(&"f".repeat(64)));

    // Mainnet with the main wallet key: refused, address only.
    let tok = mode::confirm_mainnet(true, |k: &str| {
        (k == mode::ENV_CONFIRM_MAINNET).then(|| mode::CONFIRM_MAINNET_VALUE.to_string())
    })
    .unwrap();
    let e = ExchangeClient::new(
        Mode::Mainnet,
        &cfg.endpoints(Mode::Mainnet),
        &creds(KEY, None),
        Some(tok),
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert_no_key(&e.to_string());
    assert_no_key(&format!("{e:?}"));
    assert_no_key(&format!("{:#}", anyhow::Error::from(e)));
}

#[test]
fn guard_errors_never_contain_the_key() {
    let main_addr = PrivateKey::parse(KEY)
        .unwrap()
        .wallet()
        .unwrap()
        .address()
        .to_hex();
    let cases: Vec<Vec<(&str, String)>> = vec![
        vec![(mode::ENV_PRIVATE_KEY, format!("{KEY}0"))],
        vec![(mode::ENV_PRIVATE_KEY, KEY[..60].to_string())],
        vec![(mode::ENV_PRIVATE_KEY, KEY.replace('a', "z"))],
        vec![(mode::ENV_PRIVATE_KEY, KEY.to_string())],
        vec![
            (mode::ENV_PRIVATE_KEY, KEY.to_string()),
            (mode::ENV_ACCOUNT_ADDRESS, main_addr.clone()),
        ],
        vec![
            (mode::ENV_PRIVATE_KEY, KEY.to_string()),
            (mode::ENV_ACCOUNT_ADDRESS, KEY.to_string()),
        ],
    ];
    for mut env in cases {
        env.push((
            mode::ENV_CONFIRM_MAINNET,
            mode::CONFIRM_MAINNET_VALUE.to_string(),
        ));
        let lookup = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        let e = mode::authorize(Mode::Paper, Some(Mode::Mainnet), true, lookup).unwrap_err();
        assert_no_key(&e.to_string());
        assert_no_key(&format!("{e:?}"));
        assert_no_key(&format!("{:#}", anyhow::Error::from(e)));
        for c in mode::mainnet_guard_report(true, lookup) {
            assert_no_key(&format!("{c:?}"));
        }
    }
}

/// `--config` pointed at an env file: the TOML error quotes the offending
/// line (with the key). `main` prints errors through `scrub_secrets`.
#[test]
fn config_parse_error_is_scrubbed() {
    let dir = tempfile::tempdir().unwrap();
    let mut raw_quoted_key = false;
    for body in [
        format!("HL_PRIVATE_KEY={KEY}\n"),
        format!("HL_PRIVATE_KEY={}\n", &KEY[2..]),
        format!("private_key = \"{KEY}\"\n"),
        format!(
            "mode = \"paper\"\nprivate_key = \"{}\"\n",
            KEY[2..].to_ascii_uppercase()
        ),
    ] {
        let p = dir.path().join("live.env");
        std::fs::write(&p, &body).unwrap();
        let e = anyhow::Error::from(Config::load(&p).unwrap_err());
        let raw = format!("{e:#}");
        raw_quoted_key |= raw.to_ascii_lowercase().contains(&KEY[2..]);
        let s = mode::scrub_secrets(&raw);
        assert_no_key(&s);
        assert!(s.contains("live.env"), "{s}");
    }
    // The scrub is what protects this path: the raw parse error does quote it.
    assert!(raw_quoted_key);
}
