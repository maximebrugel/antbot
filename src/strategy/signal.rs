//! Signal state machine:
//! Flat -> Entering (ALO resting, reprice, timeout) -> InPosition ->
//! Exiting (maker first, taker fallback near the 8h deadline) -> Flat (cooldown).
//!
//! TODO(step4).
