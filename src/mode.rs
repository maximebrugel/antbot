//! Mode guard and credential loading.
//!
//! Rules:
//! * `paper` never requires (nor even reads) a private key and never gets an
//!   exchange URL (see [`crate::config::Config::endpoints`]).
//! * `testnet` requires `HL_PRIVATE_KEY`; `HL_ACCOUNT_ADDRESS` is optional.
//! * `mainnet` trades **real funds**. It is refused, before any network call,
//!   unless ALL of these hold ([`authorize`]):
//!   1. the resolved mode is mainnet (`--mode mainnet` or `mode = "mainnet"`)
//!      AND the `--mainnet` CLI flag is passed;
//!   2. `ANTBOT_CONFIRM_MAINNET=yes-i-understand` (exact value);
//!   3. `HL_PRIVATE_KEY` is set and is a valid secp256k1 key (the API / agent
//!      wallet key);
//!   4. `HL_ACCOUNT_ADDRESS` is set and well-formed: the MAIN account. An
//!      agent wallet holds no funds, so querying its own address would show
//!      an empty account;
//!   5. the address derived from the key differs from `HL_ACCOUNT_ADDRESS`.
//!      If they are equal, the key is the MAIN wallet key, which can also
//!      withdraw funds: mainnet refuses it and asks for an API wallet
//!      (Hyperliquid UI: More > API), which can trade but cannot withdraw.
//! * Passing `--mainnet` while the resolved mode is not mainnet is an error, to
//!   avoid an operator believing they run on mainnet (or vice versa).
//! * The exchange client additionally requires the [`MainnetConfirmed`] token
//!   for any mainnet URL and refuses a main-wallet key on mainnet itself
//!   (defence in depth, see [`crate::hl::exchange`]).
//!
//! The private key lives in a [`PrivateKey`]: no `Display`, a redacting
//! `Debug`, zeroed on drop. Error messages never contain it.
//!
//! Every credential variable may instead name a file with `<VAR>_FILE`
//! (Docker secrets, see [`crate::secrets`]): `HL_PRIVATE_KEY_FILE`,
//! `HL_ACCOUNT_ADDRESS_FILE`, `HL_VAULT_ADDRESS_FILE`. Setting both forms is
//! an error, as is a missing / unreadable / empty file.
//!
//! Sub-accounts / vaults: `HL_VAULT_ADDRESS` (or `[live].vault_address`)
//! makes every signed action carry `vaultAddress`, and every query and user
//! feed target that address ([`Credentials::trading_address`]). On mainnet
//! the signer must then differ from both the main account and the vault.
//! Paper mode validates the format but never signs anything.
//!
//! All functions take the environment as a lookup closure so they are pure and
//! testable without mutating the process environment.

use std::fmt;

use zeroize::{Zeroize, Zeroizing};

use crate::config::Mode;
use crate::hl::signing::{Address, Wallet};
use crate::secrets::{env_or_file, SecretError};

pub const ENV_PRIVATE_KEY: &str = "HL_PRIVATE_KEY";
pub const ENV_ACCOUNT_ADDRESS: &str = "HL_ACCOUNT_ADDRESS";
/// Sub-account / vault traded on behalf of (overrides `[live].vault_address`).
pub const ENV_VAULT_ADDRESS: &str = "HL_VAULT_ADDRESS";
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
    #[error(
        "{ENV_PRIVATE_KEY} is malformed (expected 0x-prefixed 32-byte hex, a valid secp256k1 key)"
    )]
    MalformedKey,
    #[error("{ENV_ACCOUNT_ADDRESS} is malformed (expected 0x-prefixed 20-byte hex)")]
    MalformedAddress,
    #[error(
        "mainnet mode refused: {ENV_ACCOUNT_ADDRESS} (the MAIN account address) is required. \
         {ENV_PRIVATE_KEY} must be an API/agent wallet key, and an agent wallet holds no funds: \
         positions and margin are queried for the main account"
    )]
    MissingAccountAddress,
    #[error(
        "mainnet mode refused: {ENV_PRIVATE_KEY} derives to {address}, which IS \
         {ENV_ACCOUNT_ADDRESS}: this is the MAIN wallet key, which can also withdraw funds. \
         Create an API wallet in the Hyperliquid UI (More > API), authorize it for the main \
         account, and put the API wallet key in {ENV_PRIVATE_KEY} instead"
    )]
    MainWalletKey { address: String },
    #[error(
        "{ENV_VAULT_ADDRESS} / live.vault_address is malformed (expected 0x-prefixed 20-byte hex)"
    )]
    MalformedVaultAddress,
    #[error(
        "mainnet mode refused: {ENV_PRIVATE_KEY} derives to {address}, which IS the vault / \
         sub-account ({ENV_VAULT_ADDRESS}). Sign with an API wallet of the master account \
         ({ENV_ACCOUNT_ADDRESS}) instead"
    )]
    VaultKey { address: String },
    /// `VAR` and `VAR_FILE` both set, or an unusable `VAR_FILE`.
    #[error("{0}")]
    Secret(SecretError),
}

impl From<SecretError> for ModeError {
    fn from(e: SecretError) -> Self {
        ModeError::Secret(e)
    }
}

/// Proof that the mainnet guard (`--mainnet` flag AND
/// `ANTBOT_CONFIRM_MAINNET=yes-i-understand`) was satisfied.
///
/// The private field means it can only be obtained from [`confirm_mainnet`];
/// the exchange client refuses any mainnet URL without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainnetConfirmed {
    _private: (),
}

/// Check the mainnet guard and return the confirmation token.
pub fn confirm_mainnet(
    mainnet_flag: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Result<MainnetConfirmed, ModeError> {
    let env_ok = env(ENV_CONFIRM_MAINNET).as_deref() == Some(CONFIRM_MAINNET_VALUE);
    if mainnet_flag && env_ok {
        Ok(MainnetConfirmed { _private: () })
    } else {
        Err(ModeError::MainnetNotConfirmed {
            flag: mainnet_flag,
            env_ok,
        })
    }
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
        Mode::Mainnet => confirm_mainnet(mainnet_flag, env).map(|_| Mode::Mainnet),
        other if mainnet_flag => Err(ModeError::StrayMainnetFlag(other)),
        other => Ok(other),
    }
}

/// Whether a signing key is needed for this mode.
pub fn requires_key(mode: Mode) -> bool {
    !matches!(mode, Mode::Paper)
}

/// A secp256k1 private key (normalised `0x` + lowercase hex). It has no
/// `Display`, its `Debug` is redacted and the bytes are zeroed on drop: the
/// only way to read it is the explicit [`PrivateKey::expose_secret`].
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateKey(String);

impl PrivateKey {
    /// Format check only (`0x` optional, 64 hex). The error never echoes the
    /// input.
    pub fn parse(raw: &str) -> Result<Self, ModeError> {
        let t = raw.trim();
        let h = t
            .strip_prefix("0x")
            .or_else(|| t.strip_prefix("0X"))
            .unwrap_or(t);
        if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ModeError::MalformedKey);
        }
        let lower = Zeroizing::new(h.to_ascii_lowercase());
        Ok(Self(format!("0x{}", lower.as_str())))
    }

    /// The key itself, for the signer only. Never log or format it.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// The signing wallet (fails on a zero / out-of-range scalar).
    pub fn wallet(&self) -> Result<Wallet, ModeError> {
        Wallet::from_hex(&self.0).map_err(|_| ModeError::MalformedKey)
    }
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateKey(<redacted>)")
    }
}

impl Drop for PrivateKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Signing credentials for live modes. `Debug` never prints the key.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// API/agent wallet key (testnet also accepts the main wallet key).
    pub private_key: PrivateKey,
    /// Main account address when trading through an agent wallet (lowercase,
    /// `0x`-prefixed). When `None` (testnet only), the address derived from
    /// the key is used for queries. Always `Some` on mainnet.
    pub account_address: Option<String>,
    /// Sub-account / vault traded on behalf of (`vaultAddress` of every
    /// signed action). `None`: the account itself is traded.
    pub vault_address: Option<Address>,
}

impl Credentials {
    /// The main account: `account_address`, else the address of the key.
    /// Both are validated by [`load_credentials`]; for a hand-built value
    /// that fails to parse this falls back to the signer (and
    /// [`crate::hl::exchange::ExchangeClient::new`] refuses it anyway).
    pub fn account(&self) -> Option<Address> {
        match &self.account_address {
            Some(a) => a.parse().ok(),
            None => self.private_key.wallet().ok().map(|w| w.address()),
        }
    }

    /// Address whose positions, orders and fills are traded and queried:
    /// the vault / sub-account when set, else the main account.
    pub fn trading_address(&self) -> Address {
        self.vault_address
            .or_else(|| self.account())
            .unwrap_or(Address([0; 20]))
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("private_key", &"<redacted>")
            .field("account_address", &self.account_address)
            .field("vault_address", &self.vault_address)
            .finish()
    }
}

fn is_hex_of_len(s: &str, bytes: usize) -> bool {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"));
    matches!(h, Some(h) if h.len() == bytes * 2 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Validate a vault / sub-account address (`0x` + 40 hex).
pub fn parse_vault_address(s: &str) -> Result<Address, ModeError> {
    Address::from_hex(s.trim()).map_err(|_| ModeError::MalformedVaultAddress)
}

/// The vault / sub-account to trade: `HL_VAULT_ADDRESS` (or
/// `HL_VAULT_ADDRESS_FILE`) when set and non-empty, else the config value
/// (`[live].vault_address`), else `None`. Format checked in every mode.
pub fn resolve_vault(
    config_vault: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Address>, ModeError> {
    if let Some(v) = env_or_file(&env, ENV_VAULT_ADDRESS)? {
        return parse_vault_address(&v).map(Some);
    }
    match config_vault.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => parse_vault_address(v).map(Some),
        None => Ok(None),
    }
}

/// Load credentials for a resolved mode. In paper mode this returns `Ok(None)`
/// without consulting the environment at all. Format checks only: the
/// mainnet-specific rules are in [`authorize`].
pub fn load_credentials(
    mode: Mode,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Credentials>, ModeError> {
    if !requires_key(mode) {
        return Ok(None);
    }
    // Zeroizing: the raw text (env or file) is wiped once parsed.
    let raw = env_or_file(&env, ENV_PRIVATE_KEY)?.ok_or(ModeError::MissingKey(mode))?;
    let private_key = PrivateKey::parse(&raw)?;
    drop(raw);
    let account_address = match env_or_file(&env, ENV_ACCOUNT_ADDRESS)? {
        Some(a) if is_hex_of_len(&a, 20) => Some(a.to_ascii_lowercase()),
        Some(_) => return Err(ModeError::MalformedAddress),
        None => None,
    };
    Ok(Some(Credentials {
        private_key,
        account_address,
        vault_address: None,
    }))
}

/// Everything `run` needs once the guard passed.
#[derive(Debug)]
pub struct Authorized {
    pub mode: Mode,
    /// `None` in paper mode.
    pub creds: Option<Credentials>,
    /// `Some` only in mainnet mode.
    pub mainnet: Option<MainnetConfirmed>,
    /// Address derived from the key (the API wallet that signs).
    pub signer: Option<Address>,
    /// Main account (the API wallet's master account).
    pub account: Option<Address>,
    /// Sub-account / vault traded on behalf of, if configured. Validated in
    /// every mode; paper mode never uses it (it never signs).
    pub vault: Option<Address>,
}

impl Authorized {
    /// One-line identity for logs and alerts (addresses only, never the key).
    pub fn identity(&self) -> Option<String> {
        let (a, s) = (self.account?, self.signer?);
        Some(identity_line(a, s, self.vault))
    }

    /// Address traded and queried (vault when set, else the main account).
    pub fn trading_address(&self) -> Option<Address> {
        self.creds.as_ref().map(Credentials::trading_address)
    }
}

/// `account A (main), signer S (API wallet)[, trading vault / sub-account V]`.
pub fn identity_line(account: Address, signer: Address, vault: Option<Address>) -> String {
    let mut s = if account == signer {
        format!("account {account} (signer = account)")
    } else {
        format!("account {account} (main), signer {signer} (API wallet)")
    };
    if let Some(v) = vault {
        s.push_str(&format!(", trading sub-account / vault {v}"));
    }
    s
}

/// The full guard: resolve the mode, load and validate the credentials, and
/// for mainnet enforce every rule of the module docs. Pure (no I/O besides
/// the `env` lookup); `run` calls it before logging or any network client.
pub fn authorize(
    config_mode: Mode,
    cli_mode: Option<Mode>,
    mainnet_flag: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Authorized, ModeError> {
    authorize_with_vault(config_mode, cli_mode, mainnet_flag, None, env)
}

/// [`authorize`] with the config's `[live].vault_address` (the env
/// `HL_VAULT_ADDRESS[_FILE]` overrides it).
pub fn authorize_with_vault(
    config_mode: Mode,
    cli_mode: Option<Mode>,
    mainnet_flag: bool,
    config_vault: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Authorized, ModeError> {
    let mode = resolve_mode(config_mode, cli_mode, mainnet_flag, &env)?;
    let vault = resolve_vault(config_vault, &env)?;
    let creds = load_credentials(mode, &env)?;
    let Some(mut c) = creds else {
        return Ok(Authorized {
            mode,
            creds: None,
            mainnet: None,
            signer: None,
            account: None,
            vault,
        });
    };
    c.vault_address = vault;
    let signer = c.private_key.wallet()?.address();
    let account = match &c.account_address {
        Some(a) => a
            .parse::<Address>()
            .map_err(|_| ModeError::MalformedAddress)?,
        None if mode == Mode::Mainnet => return Err(ModeError::MissingAccountAddress),
        None => signer,
    };
    let mainnet = if mode == Mode::Mainnet {
        if signer == account {
            return Err(ModeError::MainWalletKey {
                address: signer.to_hex(),
            });
        }
        if vault == Some(signer) {
            return Err(ModeError::VaultKey {
                address: signer.to_hex(),
            });
        }
        Some(confirm_mainnet(mainnet_flag, &env)?)
    } else {
        None
    };
    Ok(Authorized {
        mode,
        creds: Some(c),
        mainnet,
        signer: Some(signer),
        account: Some(account),
        vault,
    })
}

/// One line of the mainnet guard report (`check-config`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardCheck {
    pub name: &'static str,
    /// `Ok(detail)` when the condition holds, `Err(why)` otherwise.
    pub result: Result<String, String>,
}

/// Evaluate every mainnet guard condition independently (unlike
/// [`authorize`], which stops at the first failure), for `check-config`.
/// Never contains the key.
pub fn mainnet_guard_report(
    mainnet_flag: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<GuardCheck> {
    mainnet_guard_report_with_vault(mainnet_flag, None, env)
}

/// [`mainnet_guard_report`] with the config's vault: two more lines
/// (vault format, API wallet != vault) when a vault is configured.
pub fn mainnet_guard_report_with_vault(
    mainnet_flag: bool,
    config_vault: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<GuardCheck> {
    let mut out = Vec::new();
    let mut push = |name, result| out.push(GuardCheck { name, result });
    push(
        "--mainnet flag",
        if mainnet_flag {
            Ok("given".to_string())
        } else {
            Err("missing".to_string())
        },
    );
    push(
        ENV_CONFIRM_MAINNET,
        match env(ENV_CONFIRM_MAINNET).as_deref() {
            Some(CONFIRM_MAINNET_VALUE) => Ok(format!("= {CONFIRM_MAINNET_VALUE}")),
            None => Err(format!("unset (must be exactly {CONFIRM_MAINNET_VALUE})")),
            Some(_) => Err(format!(
                "set to another value (must be exactly {CONFIRM_MAINNET_VALUE})"
            )),
        },
    );
    let signer = match env_or_file(&env, ENV_PRIVATE_KEY) {
        Err(e) => Err(e.to_string()),
        Ok(None) => Err(ModeError::MissingKey(Mode::Mainnet).to_string()),
        Ok(Some(raw)) => PrivateKey::parse(&raw)
            .and_then(|k| k.wallet())
            .map(|w| w.address())
            .map_err(|e| e.to_string()),
    };
    push(
        ENV_PRIVATE_KEY,
        signer
            .as_ref()
            .map(|s| format!("valid, signer (API wallet) {s}"))
            .map_err(Clone::clone),
    );
    let account = match env_or_file(&env, ENV_ACCOUNT_ADDRESS) {
        Err(e) => Err(e.to_string()),
        Ok(None) => Err("unset (required on mainnet: the MAIN account address)".to_string()),
        Ok(Some(a)) if is_hex_of_len(&a, 20) => a
            .parse::<Address>()
            .map_err(|_| ModeError::MalformedAddress.to_string()),
        Ok(Some(_)) => Err(ModeError::MalformedAddress.to_string()),
    };
    push(
        ENV_ACCOUNT_ADDRESS,
        account
            .as_ref()
            .map(|a| format!("main account {a}"))
            .map_err(Clone::clone),
    );
    push(
        "API wallet != main account",
        match (&signer, &account) {
            (Ok(s), Ok(a)) if s == a => Err(ModeError::MainWalletKey {
                address: s.to_hex(),
            }
            .to_string()),
            (Ok(_), Ok(_)) => Ok("yes (the key cannot withdraw)".to_string()),
            _ => Err("cannot check (key or account missing / invalid)".to_string()),
        },
    );
    let vault = resolve_vault(config_vault, &env);
    if !matches!(vault, Ok(None)) {
        push(
            ENV_VAULT_ADDRESS,
            vault
                .as_ref()
                .map(|v| {
                    format!(
                        "trading sub-account / vault {}",
                        v.map_or_else(String::new, |v| v.to_hex())
                    )
                })
                .map_err(ToString::to_string),
        );
        push(
            "API wallet != vault",
            match (&signer, &vault) {
                (Ok(s), Ok(Some(v))) if s == v => Err(ModeError::VaultKey {
                    address: s.to_hex(),
                }
                .to_string()),
                (Ok(_), Ok(Some(_))) => Ok("yes".to_string()),
                _ => Err("cannot check (key or vault missing / invalid)".to_string()),
            },
        );
    }
    out
}

/// Replace every run of 64+ hex digits (a private key, with or without
/// `0x`) by `<redacted-hex>`. Last line of defence for error text that may
/// echo user input (e.g. a TOML parse error quoting the offending line when
/// the config path points at an env file).
pub fn scrub_secrets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 64 {
            out.push_str("<redacted-hex>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
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
    fn mainnet_token_requires_flag_and_env() {
        let env = env_of(&[(ENV_CONFIRM_MAINNET, CONFIRM_MAINNET_VALUE)]);
        assert!(confirm_mainnet(true, &env).is_ok());
        assert!(confirm_mainnet(false, &env).is_err());
        assert!(confirm_mainnet(true, no_env).is_err());
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
        assert_eq!(c.private_key.expose_secret(), DUMMY_KEY);
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

    // ---- full guard (`authorize`) -------------------------------------

    /// A second, unrelated key: its address plays the MAIN account.
    const MAIN_KEY: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";

    fn addr_of(key: &str) -> String {
        Wallet::from_hex(key).unwrap().address().to_hex()
    }

    /// Every guard satisfied: flag passed separately, this is the env.
    fn mainnet_env() -> Vec<(&'static str, String)> {
        vec![
            (ENV_CONFIRM_MAINNET, CONFIRM_MAINNET_VALUE.to_string()),
            (ENV_PRIVATE_KEY, DUMMY_KEY.to_string()),
            (ENV_ACCOUNT_ADDRESS, addr_of(MAIN_KEY)),
        ]
    }

    fn env_vec(v: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = v.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        move |k| m.get(k).cloned()
    }

    fn without(name: &str) -> Vec<(&'static str, String)> {
        mainnet_env()
            .into_iter()
            .filter(|(k, _)| *k != name)
            .collect()
    }

    fn with(name: &'static str, value: &str) -> Vec<(&'static str, String)> {
        let mut v = without(name);
        v.push((name, value.to_string()));
        v
    }

    fn auth(flag: bool, env: Vec<(&'static str, String)>) -> Result<Authorized, ModeError> {
        authorize(Mode::Paper, Some(Mode::Mainnet), flag, env_vec(env))
    }

    #[test]
    fn mainnet_accept_path() {
        let a = auth(true, mainnet_env()).unwrap();
        assert_eq!(a.mode, Mode::Mainnet);
        assert!(a.mainnet.is_some());
        assert_eq!(a.signer.unwrap().to_hex(), addr_of(DUMMY_KEY));
        assert_eq!(a.account.unwrap().to_hex(), addr_of(MAIN_KEY));
        let c = a.creds.as_ref().unwrap();
        assert_eq!(c.private_key.expose_secret(), DUMMY_KEY);
        let id = a.identity().unwrap();
        assert!(id.contains(&addr_of(MAIN_KEY)) && id.contains(&addr_of(DUMMY_KEY)));
        // Mode from the config file works the same way.
        let a = authorize(Mode::Mainnet, None, true, env_vec(mainnet_env())).unwrap();
        assert_eq!(a.mode, Mode::Mainnet);
    }

    #[test]
    fn mainnet_refused_without_flag() {
        assert_eq!(
            auth(false, mainnet_env()).unwrap_err(),
            ModeError::MainnetNotConfirmed {
                flag: false,
                env_ok: true
            }
        );
        assert!(authorize(Mode::Mainnet, None, false, env_vec(mainnet_env())).is_err());
    }

    #[test]
    fn mainnet_refused_without_or_with_wrong_confirm_env() {
        assert_eq!(
            auth(true, without(ENV_CONFIRM_MAINNET)).unwrap_err(),
            ModeError::MainnetNotConfirmed {
                flag: true,
                env_ok: false
            }
        );
        for v in ["yes", "YES-I-UNDERSTAND", " yes-i-understand", "1", ""] {
            assert!(
                matches!(
                    auth(true, with(ENV_CONFIRM_MAINNET, v)),
                    Err(ModeError::MainnetNotConfirmed { env_ok: false, .. })
                ),
                "{v:?}"
            );
        }
    }

    #[test]
    fn mainnet_refused_without_key() {
        assert_eq!(
            auth(true, without(ENV_PRIVATE_KEY)).unwrap_err(),
            ModeError::MissingKey(Mode::Mainnet)
        );
        assert_eq!(
            auth(true, with(ENV_PRIVATE_KEY, "   ")).unwrap_err(),
            ModeError::MissingKey(Mode::Mainnet)
        );
    }

    #[test]
    fn mainnet_refused_with_malformed_key() {
        let zero = format!("0x{}", "0".repeat(64));
        // Above the secp256k1 group order: right length, not a valid key.
        let over = format!("0x{}", "f".repeat(64));
        for k in [
            "0x1234",
            &DUMMY_KEY[..65],
            &format!("{DUMMY_KEY}00"),
            &DUMMY_KEY.replace('a', "g"),
            &zero,
            &over,
        ] {
            assert_eq!(
                auth(true, with(ENV_PRIVATE_KEY, k)).unwrap_err(),
                ModeError::MalformedKey,
                "{k}"
            );
        }
    }

    #[test]
    fn mainnet_refused_without_account_address() {
        let e = auth(true, without(ENV_ACCOUNT_ADDRESS)).unwrap_err();
        assert_eq!(e, ModeError::MissingAccountAddress);
        assert!(e.to_string().contains("MAIN account"), "{e}");
        assert_eq!(
            auth(true, with(ENV_ACCOUNT_ADDRESS, "")).unwrap_err(),
            ModeError::MissingAccountAddress
        );
        // Testnet keeps HL_ACCOUNT_ADDRESS optional (account = signer).
        let a = authorize(
            Mode::Testnet,
            None,
            false,
            env_vec(without(ENV_ACCOUNT_ADDRESS)),
        )
        .unwrap();
        assert_eq!(a.account, a.signer);
        assert!(a.mainnet.is_none());
    }

    #[test]
    fn mainnet_refused_with_malformed_account_address() {
        for a in [
            "0xdead",
            "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
            "0xzz00000000000000000000000000000000000000",
        ] {
            assert_eq!(
                auth(true, with(ENV_ACCOUNT_ADDRESS, a)).unwrap_err(),
                ModeError::MalformedAddress,
                "{a}"
            );
        }
    }

    #[test]
    fn mainnet_refuses_main_wallet_key() {
        // HL_ACCOUNT_ADDRESS == address of HL_PRIVATE_KEY (any case).
        for a in [
            addr_of(DUMMY_KEY),
            addr_of(DUMMY_KEY).to_ascii_uppercase().replace("0X", "0x"),
        ] {
            let e = auth(true, with(ENV_ACCOUNT_ADDRESS, &a)).unwrap_err();
            assert_eq!(
                e,
                ModeError::MainWalletKey {
                    address: addr_of(DUMMY_KEY)
                }
            );
            let msg = e.to_string();
            assert!(
                msg.contains("MAIN wallet key") && msg.contains("More > API"),
                "{msg}"
            );
            assert!(!msg.contains(&DUMMY_KEY[2..]), "{msg}");
        }
        // Testnet still accepts the main wallet key.
        let env = env_vec(vec![
            (ENV_PRIVATE_KEY, DUMMY_KEY.to_string()),
            (ENV_ACCOUNT_ADDRESS, addr_of(DUMMY_KEY)),
        ]);
        assert!(authorize(Mode::Testnet, None, false, env).is_ok());
    }

    #[test]
    fn stray_mainnet_flag_refused_by_authorize() {
        for m in [Mode::Paper, Mode::Testnet] {
            assert_eq!(
                authorize(m, None, true, env_vec(mainnet_env())).unwrap_err(),
                ModeError::StrayMainnetFlag(m)
            );
        }
    }

    #[test]
    fn paper_authorize_reads_no_credentials() {
        let only_mode_vars = |k: &str| -> Option<String> {
            assert!(
                !k.starts_with(ENV_PRIVATE_KEY) && !k.starts_with(ENV_ACCOUNT_ADDRESS),
                "paper read {k}"
            );
            None
        };
        let a = authorize(Mode::Paper, None, false, only_mode_vars).unwrap();
        assert_eq!(a.mode, Mode::Paper);
        assert!(a.creds.is_none() && a.mainnet.is_none() && a.signer.is_none());
    }

    #[test]
    fn guard_report_lists_every_condition() {
        let r = mainnet_guard_report(true, env_vec(mainnet_env()));
        assert_eq!(r.len(), 5);
        assert!(r.iter().all(|c| c.result.is_ok()), "{r:?}");

        let r = mainnet_guard_report(false, no_env);
        assert!(r.iter().all(|c| c.result.is_err()), "{r:?}");

        // Each condition is evaluated independently.
        let r = mainnet_guard_report(
            true,
            env_vec(with(ENV_ACCOUNT_ADDRESS, &addr_of(DUMMY_KEY))),
        );
        let failed: Vec<_> = r
            .iter()
            .filter(|c| c.result.is_err())
            .map(|c| c.name)
            .collect();
        assert_eq!(failed, vec!["API wallet != main account"]);
        let r = mainnet_guard_report(true, env_vec(without(ENV_CONFIRM_MAINNET)));
        let failed: Vec<_> = r
            .iter()
            .filter(|c| c.result.is_err())
            .map(|c| c.name)
            .collect();
        assert_eq!(failed, vec![ENV_CONFIRM_MAINNET]);
    }

    // ---- the key never shows up in text -------------------------------

    /// Every way the guard can render text (errors, Debug, report), for
    /// valid and invalid inputs, must not contain the key (any case, with or
    /// without 0x).
    #[test]
    fn key_never_appears_in_formatted_output() {
        let needle = DUMMY_KEY[2..].to_ascii_lowercase();
        let check = |s: String| {
            assert!(!s.to_ascii_lowercase().contains(&needle), "key leaked: {s}");
        };
        let upper = format!("0X{}", DUMMY_KEY[2..].to_ascii_uppercase());
        let envs = vec![
            mainnet_env(),
            with(ENV_PRIVATE_KEY, &upper),
            with(ENV_PRIVATE_KEY, &DUMMY_KEY[2..]),
            with(ENV_PRIVATE_KEY, &format!("{DUMMY_KEY}ff")), // malformed, contains key
            with(ENV_ACCOUNT_ADDRESS, &addr_of(DUMMY_KEY)),
            with(ENV_ACCOUNT_ADDRESS, DUMMY_KEY), // key pasted as the address
            without(ENV_ACCOUNT_ADDRESS),
            without(ENV_CONFIRM_MAINNET),
        ];
        for env in envs {
            for flag in [true, false] {
                for (cfg, cli) in [
                    (Mode::Paper, Some(Mode::Mainnet)),
                    (Mode::Testnet, None),
                    (Mode::Paper, None),
                ] {
                    match authorize(cfg, cli, flag, env_vec(env.clone())) {
                        Ok(a) => {
                            check(format!("{a:?}"));
                            check(format!("{:?}", a.creds));
                            check(format!("{:#?}", a.creds));
                            if let Some(c) = &a.creds {
                                check(format!("{:?}", c.private_key));
                                check(format!("{:?}", c.private_key.wallet().unwrap()));
                            }
                            check(a.identity().unwrap_or_default());
                        }
                        Err(e) => {
                            check(e.to_string());
                            check(format!("{e:?}"));
                            check(format!("{:#}", anyhow::Error::from(e.clone())));
                        }
                    }
                    check(format!(
                        "{:?}",
                        load_credentials(Mode::Testnet, env_vec(env.clone()))
                    ));
                }
                for c in mainnet_guard_report(flag, env_vec(env.clone())) {
                    check(format!("{c:?}"));
                }
            }
        }
    }

    // ---- *_FILE secrets ------------------------------------------------

    fn write_tmp(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p.to_str().unwrap().to_string()
    }

    #[test]
    fn credentials_from_files_with_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let key = write_tmp(&dir, "key", &format!("{DUMMY_KEY}\n"));
        let acct = write_tmp(&dir, "acct", &format!("  {}\r\n", addr_of(MAIN_KEY)));
        let mut env = without(ENV_PRIVATE_KEY);
        env.retain(|(k, _)| *k != ENV_ACCOUNT_ADDRESS);
        env.push(("HL_PRIVATE_KEY_FILE", key));
        env.push(("HL_ACCOUNT_ADDRESS_FILE", acct));
        let a = auth(true, env).unwrap();
        assert_eq!(a.mode, Mode::Mainnet);
        let c = a.creds.as_ref().unwrap();
        assert_eq!(c.private_key.expose_secret(), DUMMY_KEY);
        assert_eq!(a.account.unwrap().to_hex(), addr_of(MAIN_KEY));
        // check-config report reads the files the same way.
        let mut env = without(ENV_PRIVATE_KEY);
        env.push((
            "HL_PRIVATE_KEY_FILE",
            write_tmp(&dir, "key2", &format!("\n{DUMMY_KEY}\n\n")),
        ));
        let r = mainnet_guard_report(true, env_vec(env));
        assert!(r.iter().all(|c| c.result.is_ok()), "{r:?}");
    }

    #[test]
    fn key_and_key_file_together_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let key = write_tmp(&dir, "key", DUMMY_KEY);
        let mut env = mainnet_env();
        env.push(("HL_PRIVATE_KEY_FILE", key));
        let e = auth(true, env.clone()).unwrap_err();
        assert!(
            matches!(&e, ModeError::Secret(SecretError::Conflict { var }) if var == ENV_PRIVATE_KEY),
            "{e:?}"
        );
        assert!(e.to_string().contains("HL_PRIVATE_KEY_FILE"), "{e}");
        assert!(!e.to_string().contains(&DUMMY_KEY[2..]));
        let r = mainnet_guard_report(true, env_vec(env));
        let k = r.iter().find(|c| c.name == ENV_PRIVATE_KEY).unwrap();
        assert!(k.result.as_ref().unwrap_err().contains("only one"), "{k:?}");
    }

    #[test]
    fn missing_or_empty_key_file_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let empty = write_tmp(&dir, "empty", "\n \n");
        let missing = dir.path().join("missing").to_str().unwrap().to_string();
        for (path, want) in [(empty, "empty"), (missing, "cannot read")] {
            let mut env = without(ENV_PRIVATE_KEY);
            env.push(("HL_PRIVATE_KEY_FILE", path.clone()));
            let e = authorize(Mode::Testnet, None, false, env_vec(env)).unwrap_err();
            let msg = e.to_string();
            assert!(
                msg.contains("HL_PRIVATE_KEY_FILE") && msg.contains(&path) && msg.contains(want),
                "{msg}"
            );
        }
    }

    #[test]
    fn key_from_file_never_appears_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let needle = DUMMY_KEY[2..].to_string();
        // Malformed key in a file (key + junk) and key pasted as an address.
        let bad_key = write_tmp(&dir, "bad", &format!("{DUMMY_KEY}ff\n"));
        let as_addr = write_tmp(&dir, "addr", DUMMY_KEY);
        let mut e1 = without(ENV_PRIVATE_KEY);
        e1.push(("HL_PRIVATE_KEY_FILE", bad_key));
        let mut e2 = without(ENV_ACCOUNT_ADDRESS);
        e2.push(("HL_ACCOUNT_ADDRESS_FILE", as_addr));
        for env in [e1, e2] {
            let err = auth(true, env.clone()).unwrap_err();
            for s in [err.to_string(), format!("{err:?}")] {
                assert!(!s.to_ascii_lowercase().contains(&needle), "{s}");
            }
            for c in mainnet_guard_report(true, env_vec(env)) {
                assert!(!format!("{c:?}").contains(&needle), "{c:?}");
            }
        }
    }

    // ---- vault / sub-account -------------------------------------------

    const VAULT: &str = "0x1719884eb866cb12b2287399b15f7db5e7d775ea";

    #[test]
    fn vault_from_env_overrides_config() {
        let v = resolve_vault(Some(VAULT), no_env).unwrap().unwrap();
        assert_eq!(v.to_hex(), VAULT);
        let other = "0x00000000000000000000000000000000000000bb";
        let env = env_of(&[(ENV_VAULT_ADDRESS, other)]);
        assert_eq!(
            resolve_vault(Some(VAULT), env).unwrap().unwrap().to_hex(),
            other
        );
        // Empty env and empty config: no vault.
        let env = env_of(&[(ENV_VAULT_ADDRESS, " ")]);
        assert_eq!(resolve_vault(Some(""), env), Ok(None));
        // From a file.
        let dir = tempfile::tempdir().unwrap();
        let f = write_tmp(
            &dir,
            "vault",
            &format!("{}\n", VAULT.to_ascii_uppercase().replace("0X", "0x")),
        );
        let env = env_of(&[("HL_VAULT_ADDRESS_FILE", &f)]);
        assert_eq!(resolve_vault(None, env).unwrap().unwrap().to_hex(), VAULT);
        // Malformed (no 0x, short, key-length) are refused.
        for bad in [&VAULT[2..], "0x1234", DUMMY_KEY] {
            assert_eq!(
                resolve_vault(Some(bad), no_env),
                Err(ModeError::MalformedVaultAddress),
                "{bad}"
            );
        }
    }

    #[test]
    fn vault_is_carried_by_credentials_and_routes_trading() {
        let mut env = mainnet_env();
        env.push((ENV_VAULT_ADDRESS, VAULT.to_string()));
        let a = auth(true, env).unwrap();
        assert_eq!(a.vault.unwrap().to_hex(), VAULT);
        let c = a.creds.as_ref().unwrap();
        assert_eq!(c.vault_address.unwrap().to_hex(), VAULT);
        assert_eq!(c.trading_address().to_hex(), VAULT);
        assert_eq!(a.trading_address().unwrap().to_hex(), VAULT);
        assert_eq!(c.account().unwrap().to_hex(), addr_of(MAIN_KEY));
        let id = a.identity().unwrap();
        assert!(
            id.contains(VAULT) && id.contains(&addr_of(MAIN_KEY)),
            "{id}"
        );
        // Without a vault the main account is traded (unchanged).
        let a = auth(true, mainnet_env()).unwrap();
        assert_eq!(a.vault, None);
        assert_eq!(
            a.creds.as_ref().unwrap().trading_address().to_hex(),
            addr_of(MAIN_KEY)
        );
        // Config vault through authorize_with_vault.
        let a = authorize_with_vault(
            Mode::Testnet,
            None,
            false,
            Some(VAULT),
            env_vec(without(ENV_ACCOUNT_ADDRESS)),
        )
        .unwrap();
        assert_eq!(a.trading_address().unwrap().to_hex(), VAULT);
    }

    #[test]
    fn mainnet_refuses_signer_equal_to_vault() {
        let mut env = mainnet_env();
        env.push((ENV_VAULT_ADDRESS, addr_of(DUMMY_KEY)));
        let e = auth(true, env.clone()).unwrap_err();
        assert_eq!(
            e,
            ModeError::VaultKey {
                address: addr_of(DUMMY_KEY)
            }
        );
        let r = mainnet_guard_report(true, env_vec(env));
        let failed: Vec<_> = r
            .iter()
            .filter(|c| c.result.is_err())
            .map(|c| c.name)
            .collect();
        assert_eq!(failed, vec!["API wallet != vault"]);
        // Testnet keeps the looser rules.
        let env = env_vec(vec![
            (ENV_PRIVATE_KEY, DUMMY_KEY.to_string()),
            (ENV_VAULT_ADDRESS, addr_of(DUMMY_KEY)),
        ]);
        assert!(authorize(Mode::Testnet, None, false, env).is_ok());
    }

    #[test]
    fn paper_validates_but_ignores_the_vault() {
        let a = authorize_with_vault(Mode::Paper, None, false, Some(VAULT), no_env).unwrap();
        assert!(a.creds.is_none());
        assert_eq!(a.trading_address(), None);
        assert_eq!(a.vault.unwrap().to_hex(), VAULT);
        assert_eq!(
            authorize_with_vault(Mode::Paper, None, false, Some("0xdead"), no_env).unwrap_err(),
            ModeError::MalformedVaultAddress
        );
        // Report: two extra lines only when a vault is configured.
        assert_eq!(mainnet_guard_report(true, env_vec(mainnet_env())).len(), 5);
        let r = mainnet_guard_report_with_vault(true, Some(VAULT), env_vec(mainnet_env()));
        assert_eq!(r.len(), 7);
        assert!(r.iter().all(|c| c.result.is_ok()), "{r:?}");
    }

    #[test]
    fn scrub_secrets_redacts_long_hex_runs() {
        let line = format!("expected integer, found `HL_PRIVATE_KEY={DUMMY_KEY}` at line 3");
        let s = scrub_secrets(&line);
        assert!(!s.contains(&DUMMY_KEY[2..]), "{s}");
        assert!(
            s.contains("<redacted-hex>") && s.contains("at line 3"),
            "{s}"
        );
        let bare = scrub_secrets(&DUMMY_KEY[2..].to_ascii_uppercase());
        assert_eq!(bare, "<redacted-hex>");
        // Addresses, oids and short hex stay readable.
        let keep = "account 0x00000000000000000000000000000000000000aa oid 123 cafe";
        assert_eq!(scrub_secrets(keep), keep);
        assert_eq!(scrub_secrets("héllo — ok"), "héllo — ok");
    }
}
