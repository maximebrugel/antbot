//! antbot — xyz:ANT HIP-3 premium mean-reversion bot for Hyperliquid.
//!
//! Development build: paper mode (default) uses live mainnet market data with
//! simulated fills and never calls `/exchange`.

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
