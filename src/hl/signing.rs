//! L1 action signing: msgpack(action) ++ nonce ++ vault flag -> keccak ->
//! phantom agent {source: "a" mainnet / "b" testnet, connectionId} signed as
//! EIP-712 (domain "Exchange", chainId 1337). Tested against official vectors.
//!
//! TODO(step3).
