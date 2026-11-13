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
