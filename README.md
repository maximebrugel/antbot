# antbot

Premium mean-reversion bot for the HIP-3 perp `xyz:ANT` on Hyperliquid (Rust / tokio).
Default mode: **paper** (live mainnet market data, simulated fills, no key, `/exchange` is never called).

> Full documentation to come. This is not investment advice.

```sh
cargo run -- check-config            # validate config/default.toml and the mode guard
cargo run -- run                     # paper trading on live mainnet data
cargo run -- replay --help           # replay / backtest over historical data
```

Stop the bot before running `antbot reset-kill --confirm`: a running bot holds
`<data_dir>/antbot.lock` and the command refuses to run while it is held.

## Monitoring

Everything is written to the data dir (`monitor.data_dir`, default `data/`, or `--data-dir`):

| file | content |
|---|---|
| `logs/antbot.log.YYYY-MM-DD` | daily rolling log (same lines as stdout; `monitor.log_max_files` kept). `RUST_LOG` sets the filter, `monitor.log_json = true` switches both outputs to JSON lines. |
| `signals.csv` | one premium sample every `timing.signal_sample_secs`: `ts_ms, ts_utc, oracle_px, mark_px, impact_bid_px, impact_ask_px, impact_mid_px, best_bid, best_ask, book_mid_px, premium_bps` (the signal, rolling mean), `raw_premium_bps`, `api_premium_bps`, `avg_ready, stale, state, position, killed` |
| `orders.csv` | every order command and its feedback: `place`, `cancel`, `resting`, `fill` (px, sz, fee, maker/taker), `done` (status, filled size, reject reason) |
| `trades.csv` | one row per round trip: entry / exit time, side, size, entry / exit px, notional, fees, funding, gross / net PnL, PnL bps, exit reason, entry / exit premium, hold time |
| `state_<mode>.json`, `risk_state.json` | persisted strategy / ledger / executor and kill-switch state |

CSV headers are written once; restarts append. If a newer version changes the columns, the old
file is renamed `<name>.<unix_ms>.old` and a new one is started.

A `heartbeat` log line every `timing.heartbeat_secs` shows state, premium (signal and raw),
position, unrealized / realized PnL, equity, kill state and feed health.

**Telegram alerts:** set `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID`. Without them, alerts are
disabled with a single log line. Alerts cover:
- startup and shutdown;
- position opened (entry filled), exit started, taker fallback, trade closed with PnL;
- kill switch, market spec changes;
- warnings and errors (resyncs, mismatches, rate limits, state save failures, fatal errors);
- a daily summary of the previous UTC day, sent after 00:00 UTC (`monitor.daily_summary`).

Sending never blocks the trading loop. Alerts go through a bounded queue. A background task
sends them, at most `monitor.telegram_max_per_min` per minute and about one per second. Alerts that
pile up meanwhile are merged into one message. A 429 response is retried after its `retry_after`,
and 5xx and transport errors are retried with backoff. If the queue overflows, alerts are dropped
and the next message reports how many were lost.
