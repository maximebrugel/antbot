//! Info REST client (`POST /info`): meta (dex), perpDexs, metaAndAssetCtxs,
//! l2Book, clearinghouseState, openOrders, userFills, candleSnapshot,
//! fundingHistory. Token-bucket rate limited, retried.
//!
//! TODO(step2): implement with reqwest + rustls.
