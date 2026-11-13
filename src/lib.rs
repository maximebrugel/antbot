//! antbot — xyz:ANT HIP-3 premium mean-reversion bot for Hyperliquid.
//!
//! Modes: paper (default) uses live mainnet market data with simulated fills
//! and never calls `/exchange`; testnet trades on the testnet; mainnet trades
//! REAL FUNDS and only starts when every guard in [`mode`] passes (`--mode
//! mainnet --mainnet`, `ANTBOT_CONFIRM_MAINNET=yes-i-understand`, an API
//! wallet key in `HL_PRIVATE_KEY` distinct from the main account in
//! `HL_ACCOUNT_ADDRESS`). The same live safety checks (notional caps,
//! post-only entries, isolated margin, kill switches) apply on testnet and
//! mainnet.

pub mod config;
pub mod engine;
pub mod exec;
pub mod hl;
pub mod mode;
pub mod monitor;
pub mod replay;
pub mod risk;
pub mod runlock;
pub mod runner;
pub mod strategy;
