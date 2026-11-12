//! Mode guard and credential loading.
//!
//! Rules:
//! * `paper` never requires (nor even reads) a private key and never gets an
//!   exchange URL (see [`crate::config::Config::endpoints`]).
//! * `testnet` requires `HL_PRIVATE_KEY`.
//! * `mainnet` is refused unless BOTH the `--mainnet` CLI flag is passed AND
//!   `ANTBOT_CONFIRM_MAINNET=yes-i-understand` is set; it also requires a key.
//! * Passing `--mainnet` while the resolved mode is not mainnet is an error, to
//!   avoid an operator believing they run on mainnet (or vice versa).
//!
//! All functions take the environment as a lookup closure so they are pure and
//! testable without mutating the process environment.

use std::fmt;

use crate::config::Mode;

pub const ENV_PRIVATE_KEY: &str = "HL_PRIVATE_KEY";
pub const ENV_ACCOUNT_ADDRESS: &str = "HL_ACCOUNT_ADDRESS";
pub const ENV_CONFIRM_MAINNET: &str = "ANTBOT_CONFIRM_MAINNET";
pub const CONFIRM_MAINNET_VALUE: &str = "yes-i-understand";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModeError {
    #[error(
        "mainnet mode refused: requires the --mainnet flag AND \
         {ENV_CONFIRM_MAINNET}={CONFIRM_MAINNET_VALUE} (flag: {flag}, env confirmed: {env_ok})"
    )]
    MainnetNotConfirmed { flag: bool, env_ok: bool },
    #[error("--mainnet flag given but the resolved mode is '{0}'; pass --mode mainnet explicitly")]
    StrayMainnetFlag(Mode),
    #[error("{ENV_PRIVATE_KEY} is required in {0} mode")]
    MissingKey(Mode),
    #[error("{ENV_PRIVATE_KEY} is malformed (expected 0x-prefixed 32-byte hex)")]
    MalformedKey,
    #[error("{ENV_ACCOUNT_ADDRESS} is malformed (expected 0x-prefixed 20-byte hex)")]
    MalformedAddress,
}

/// Resolve the effective mode from the config value, an optional CLI override,
/// the `--mainnet` flag and the confirmation env var.
pub fn resolve_mode(
    config_mode: Mode,
    cli_mode: Option<Mode>,
    mainnet_flag: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Mode, ModeError> {
    let mode = cli_mode.unwrap_or(config_mode);
    match mode {
        Mode::Mainnet => {
            let env_ok = env(ENV_CONFIRM_MAINNET).as_deref() == Some(CONFIRM_MAINNET_VALUE);
            if mainnet_flag && env_ok {
                Ok(Mode::Mainnet)
            } else {
                Err(ModeError::MainnetNotConfirmed {
                    flag: mainnet_flag,
                    env_ok,
                })
            }
        }
        other if mainnet_flag => Err(ModeError::StrayMainnetFlag(other)),
        other => Ok(other),
    }
}

/// Whether a signing key is needed for this mode.
pub fn requires_key(mode: Mode) -> bool {
    !matches!(mode, Mode::Paper)
}

/// Signing credentials for live modes. `Debug` never prints the key.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// 0x-prefixed hex secp256k1 private key (API/agent wallet or main wallet).
    pub private_key: String,
    /// Main account address when trading through an agent wallet. When `None`,
    /// the address derived from the key is used for queries.
    pub account_address: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("private_key", &"<redacted>")
            .field("account_address", &self.account_address)
            .finish()
    }
}

fn is_hex_of_len(s: &str, bytes: usize) -> bool {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"));
    matches!(h, Some(h) if h.len() == bytes * 2 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Load credentials for a resolved mode. In paper mode this returns `Ok(None)`
/// without consulting the environment at all.
pub fn load_credentials(
    mode: Mode,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Credentials>, ModeError> {
    if !requires_key(mode) {
        return Ok(None);
    }
    let key = env(ENV_PRIVATE_KEY)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or(ModeError::MissingKey(mode))?;
    let key = if key.starts_with("0x") || key.starts_with("0X") {
        key
    } else {
        format!("0x{key}")
    };
    if !is_hex_of_len(&key, 32) {
        return Err(ModeError::MalformedKey);
    }
    let account_address = match env(ENV_ACCOUNT_ADDRESS)
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
    {
        Some(a) if is_hex_of_len(&a, 20) => Some(a.to_ascii_lowercase()),
        Some(_) => return Err(ModeError::MalformedAddress),
        None => None,
    };
    Ok(Some(Credentials {
        private_key: key,
        account_address,
    }))
}

/// Process-environment lookup used by `main`.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const DUMMY_KEY: &str = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn paper_is_default_and_needs_nothing() {
        assert_eq!(
            resolve_mode(Mode::Paper, None, false, no_env),
            Ok(Mode::Paper)
        );
        assert!(!requires_key(Mode::Paper));
    }

    #[test]
    fn paper_never_reads_env_for_credentials() {
        let panicking = |k: &str| -> Option<String> { panic!("paper read env var {k}") };
        assert_eq!(load_credentials(Mode::Paper, panicking), Ok(None));
    }

    #[test]
    fn paper_ignores_key_even_if_present() {
        let env = env_of(&[(ENV_PRIVATE_KEY, DUMMY_KEY)]);
        assert_eq!(load_credentials(Mode::Paper, env), Ok(None));
    }

    #[test]
    fn cli_overrides_config_mode() {
        assert_eq!(
            resolve_mode(Mode::Paper, Some(Mode::Testnet), false, no_env),
            Ok(Mode::Testnet)
        );
        assert_eq!(
            resolve_mode(Mode::Testnet, Some(Mode::Paper), false, no_env),
            Ok(Mode::Paper)
        );
    }

    #[test]
    fn mainnet_refused_without_flag_and_env() {
        let err = resolve_mode(Mode::Paper, Some(Mode::Mainnet), false, no_env).unwrap_err();
        assert_eq!(
            err,
            ModeError::MainnetNotConfirmed {
                flag: false,
                env_ok: false
            }
        );
    }

    #[test]
    fn mainnet_refused_with_flag_only() {
        let err = resolve_mode(Mode::Paper, Some(Mode::Mainnet), true, no_env).unwrap_err();
        assert_eq!(
            err,
            ModeError::MainnetNotConfirmed {
                flag: true,
                env_ok: false
            }
        );
    }

    #[test]
    fn mainnet_refused_with_env_only() {
        let env = env_of(&[(ENV_CONFIRM_MAINNET, CONFIRM_MAINNET_VALUE)]);
        let err = resolve_mode(Mode::Paper, Some(Mode::Mainnet), false, env).unwrap_err();
        assert_eq!(
            err,
            ModeError::MainnetNotConfirmed {
                flag: false,
                env_ok: true
            }
        );
    }

    #[test]
    fn mainnet_refused_with_wrong_env_value() {
        for v in [
            "yes",
            "YES-I-UNDERSTAND",
            "yes-i-understand ",
            "true",
            "1",
            "",
        ] {
            let env = env_of(&[(ENV_CONFIRM_MAINNET, v)]);
            assert!(
                resolve_mode(Mode::Paper, Some(Mode::Mainnet), true, env).is_err(),
                "value {v:?} must not confirm mainnet"
            );
        }
    }

    #[test]
    fn mainnet_in_config_file_still_needs_flag_and_env() {
        assert!(resolve_mode(Mode::Mainnet, None, false, no_env).is_err());
        let env = env_of(&[(ENV_CONFIRM_MAINNET, CONFIRM_MAINNET_VALUE)]);
        assert!(resolve_mode(Mode::Mainnet, None, false, env).is_err());
    }

    #[test]
    fn mainnet_accepted_with_flag_and_env() {
        let env = env_of(&[(ENV_CONFIRM_MAINNET, CONFIRM_MAINNET_VALUE)]);
        assert_eq!(
            resolve_mode(Mode::Paper, Some(Mode::Mainnet), true, env),
            Ok(Mode::Mainnet)
        );
    }

    #[test]
    fn stray_mainnet_flag_is_rejected() {
        assert_eq!(
            resolve_mode(Mode::Paper, None, true, no_env),
            Err(ModeError::StrayMainnetFlag(Mode::Paper))
        );
        assert_eq!(
            resolve_mode(Mode::Paper, Some(Mode::Testnet), true, no_env),
            Err(ModeError::StrayMainnetFlag(Mode::Testnet))
        );
    }

    #[test]
    fn live_modes_require_key() {
        for m in [Mode::Testnet, Mode::Mainnet] {
            assert!(requires_key(m));
            assert_eq!(load_credentials(m, no_env), Err(ModeError::MissingKey(m)));
            let env = env_of(&[(ENV_PRIVATE_KEY, "  ")]);
            assert_eq!(load_credentials(m, env), Err(ModeError::MissingKey(m)));
        }
    }

    #[test]
    fn key_format_validated() {
        let env = env_of(&[(ENV_PRIVATE_KEY, "0x1234")]);
        assert_eq!(
            load_credentials(Mode::Testnet, env),
            Err(ModeError::MalformedKey)
        );
        // Missing 0x prefix is tolerated and normalised.
        let env = env_of(&[(ENV_PRIVATE_KEY, &DUMMY_KEY[2..])]);
        let c = load_credentials(Mode::Testnet, env).unwrap().unwrap();
        assert_eq!(c.private_key, DUMMY_KEY);
        assert_eq!(c.account_address, None);
    }

    #[test]
    fn account_address_optional_and_validated() {
        let addr = "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01";
        let env = env_of(&[(ENV_PRIVATE_KEY, DUMMY_KEY), (ENV_ACCOUNT_ADDRESS, addr)]);
        let c = load_credentials(Mode::Testnet, env).unwrap().unwrap();
        assert_eq!(
            c.account_address.as_deref(),
            Some(addr.to_ascii_lowercase().as_str())
        );

        let env = env_of(&[
            (ENV_PRIVATE_KEY, DUMMY_KEY),
            (ENV_ACCOUNT_ADDRESS, "0xdead"),
        ]);
        assert_eq!(
            load_credentials(Mode::Testnet, env),
            Err(ModeError::MalformedAddress)
        );
    }

    #[test]
    fn debug_redacts_key() {
        let env = env_of(&[(ENV_PRIVATE_KEY, DUMMY_KEY)]);
        let c = load_credentials(Mode::Testnet, env).unwrap().unwrap();
        let s = format!("{c:?}");
        assert!(!s.contains(&DUMMY_KEY[2..]), "{s}");
        assert!(s.contains("redacted"));
    }
}
