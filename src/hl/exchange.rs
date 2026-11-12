//! Exchange client (`POST /exchange`): order (ALO / IOC / reduce-only),
//! cancel, cancelByCloid, updateLeverage (isolated).
//!
//! Safety: never constructed in paper mode; refuses the mainnet URL unless
//! the mainnet guard (see `crate::mode`) was satisfied.
//!
//! TODO(step3).
