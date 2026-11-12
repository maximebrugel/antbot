//! Websocket client: l2Book / activeAssetCtx (+ userFills / orderUpdates in
//! live modes), heartbeat, reconnect with exponential backoff, resubscribe,
//! stale detection with REST fallback.
//!
//! TODO(step2): implement with tokio-tungstenite + rustls.
