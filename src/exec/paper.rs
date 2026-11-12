//! Paper executor: live mainnet market data, simulated post-only ALO fills
//! (fill when the book trades through / at our price), IOC taker fills by
//! walking the book, fees from config. Never touches /exchange.
//!
//! TODO(step5).
