# antbot

A premium mean-reversion bot for the HIP-3 perpetual **`xyz:ANT`** on Hyperliquid, written in Rust (tokio).

`xyz:ANT` is a builder-deployed perp on the `xyz` dex. It tracks pre-IPO Anthropic and has a funding multiplier of 0.005. Its order book often trades away from the oracle. When the gap (the **premium**) is extreme, the bot posts maker-only orders on the side that profits if the gap closes. It holds the position for at most 8 hours and exits earlier if the premium converges.

**Default mode: paper.** Paper mode uses live mainnet market data and simulated fills. It needs no key and never calls `/exchange`. Mainnet trading (real funds) is available behind an explicit multi-condition guard: see [Running on mainnet (real funds)](#running-on-mainnet-real-funds).

> **This is not financial advice.** It is experimental software built on a very short history (17 days, 24 backtest trades) for an illiquid pre-IPO market. You can lose everything you put through it. See [Risks and limitations](#risks-and-limitations).

## Contents

- [What it does](#what-it-does)
- [Strategy](#strategy)
- [Architecture](#architecture)
- [Build and run](#build-and-run)
- [Modes and the mainnet guard](#modes-and-the-mainnet-guard)
- [Environment variables](#environment-variables)
- [Hyperliquid API / agent wallet](#hyperliquid-api--agent-wallet)
- [Running on mainnet (real funds)](#running-on-mainnet-real-funds)
- [Configuration reference](#configuration-reference)
- [Replay / backtest](#replay--backtest)
- [Docker deployment on a VPS](#docker-deployment-on-a-vps)
- [Monitoring](#monitoring)
- [Kill rules](#kill-rules)
- [Risks and limitations](#risks-and-limitations)

## What it does

- **Resolves the asset at startup.** It reads `perpDexs` and `meta {"dex":"xyz"}` and computes the HIP-3 asset id as `100000 + perp_dex_index * 10000 + index_in_meta`. On mainnet today, `xyz:ANT` resolves to dex index 1 and meta index 138, so asset id 110138. It also reads `szDecimals`, max leverage and the fee/growth-mode fields.
- **Streams market data** over the websocket: `l2Book`, `activeAssetCtx` (oracle, mark, impact prices), and in paper mode public `trades`. It reconnects with exponential backoff and falls back to REST polling (`metaAndAssetCtxs` + `l2Book`) while the websocket is stale.
- **Computes the premium** continuously, averages it over a rolling window (the backtested signal), and runs a state machine: flat → entering → in position → exiting → cooldown.
- **Places orders.** Entries are maker-only ALO (post-only) limit orders on isolated margin, 1x by default. Exits go maker first, with an IOC taker fallback near the deadline. Prices respect the tick and 5-significant-figure rules, and sizes respect `szDecimals`.
- **Enforces kill rules:** hit rate, persistent premium, spec change, daily loss and max drawdown. A kill is persisted and needs a manual reset.
- **Persists state** (position, PnL, trade history, kill state) in a data directory, so a restart resumes where it stopped.
- **Records and alerts.** It writes structured logs, CSV journals (premium samples, orders, trades) and optional Telegram alerts, and answers read-only Telegram commands (`/pnl`, `/trades`, `/help`) from its private chat.
- **Replays history.** `antbot replay` runs the same strategy and fill model over historical hourly data.

## Strategy

### Signal

```
premium_bps = (0.5 * (impact_bid_px + impact_ask_px) / oracle_px - 1) * 10_000
```

This is the same definition Hyperliquid uses for `fundingHistory.premium` and the research backtest. `activeAssetCtx.impactPxs` provides the impact prices. If `strategy.premium_source = "book_mid"`, the l2Book mid is used instead.

The signal is the rolling mean of the premium over `strategy.premium_avg_secs` (3600 s by default, one sample per 5 s slot). This matches the backtested hourly premium, but it is evaluated continuously rather than once an hour. Nothing trades until 80% of the window has been sampled. Samples persist across restarts.

| rule | default |
|---|---|
| Entry | \|signal\| ≥ 50 bps. premium > 0 → **short**, premium < 0 → **long** (fade the premium) |
| Order type | maker-only ALO limit at the touch. Re-quoted when the price drifts ≥ 5 bps (at most every 20 s). Abandoned after 15 min |
| Position | one at a time, isolated margin, 1x |
| Size | 2,500 USD notional (hard cap 10,000 USD, enforced in code) |
| Early exit | signal back to ≤ 10 bps on the faded side (or crossed). Can be turned off |
| Max hold | 8 h. The maker exit starts at 7 h 35 min, and IOC taker fallback is used in the last 15 min |
| Cooldown | 1 h after an exit (as in the backtest) |

### Research numbers

These figures come from the research notes (`writeup_v2`, `sizing`). They use read-only Hyperliquid data from 2026-10-26 to 2026-11-12, so only about **17 days** of `xyz:ANT` history.

- **Premium behaviour:** mean |premium| is about 24–39 bps, and funding is close to zero (−0.8% to +0.2% annualized). Over 411 hourly samples, when |premium| ≥ 50 bps, it was closer to 0 eight hours later 94.7% of the time (n = 75), and it fell from 87.7 to 29.0 bps on average.
- **Backtest, S2 ANT `8h_fixed`** (enter at |premium| ≥ 50 bps, hold 8 h, maker/maker growth-mode fees):

| metric | value |
|---|---|
| trades | 24 (about 43 per month), never overlapping |
| winners (full PnL) | 75% |
| mean net PnL per trade | **+128.9 bps** (median +106.1, p5 −170) |
| mean basis-only PnL (oracle assumed constant) | +89.3 bps |
| max cumulative drawdown | 418 bps |
| total at 10k USD per trade | +3,095 USD full / +2,143 USD basis-only |
| 4h_reeval / 4h_fixed variants | +59.2 / +25.0 bps per trade (worse) |

- **Directional luck:** the oracle moved *in the trade's direction* by about 43 bps per trade. Roughly 40 bps of the full PnL per trade is therefore directional and lucky. The basis-only figure (+89 bps) is the more prudent estimate. Shorts made +168 bps (n = 15) and longs +63 bps (n = 9).
- **Liquidity:** the book is thin. There was about 31–35k USD over 20 levels per side and about 2.7k USD in the top 3 ask levels (a single snapshot). Taker impact per leg was about 2.5 bps at 5k USD, 3.6 bps at 10k and 7.7 bps at 25k. Beyond about 30k USD the visible book cannot fill the order. Capacity becomes the limit from about 10k USD.
- **Fees:** tier 0, HIP-3 `deployerFeeScale` 1.0 with **growth mode enabled**:

| case | maker | taker | round trip |
|---|---|---|---|
| growth mode, maker/maker (base) | 0.003% | 0.009% | 0.6 bp |
| growth mode, maker/taker | | | 1.2 bp |
| standard (growth mode off), maker/maker | 0.030% | 0.090% | 6 bp |
| standard, maker/taker | | | 12 bp |
| standard, taker/taker | | | 18 bp |

- **Adverse excursion:** for the 8 h trades, it was 4.4% at p95 and 5.3% at worst. At 1x isolated margin this is far from liquidation. The maintenance margin is about 5% at max leverage 10x.
- **Sizing recommendation:** start at **2,500 USD notional, 1x** (2x maximum), with a **10,000 USD maximum** because the visible book cannot absorb more. Those are the defaults here.
- **Monte Carlo:** a bootstrap of the 24 trades suggests very high monthly returns, with "0.1% losing months". **Those numbers are not expectations.** They extrapolate 17 days and 24 trades with a large directional component, and the i.i.d. bootstrap understates the variance. Treat them as an optimistic ceiling at best.

### What the bot's own replay shows

`antbot replay` runs the bot's strategy and paper fill model over the same 411 hours at 2,500 USD notional. Exits happen up to an hour earlier than in the research rule (see [Replay](#replay--backtest)).

| configuration | trades | hit rate (\|p\| → 0) | PnL > 0 | mean net bps | PnL USD | max DD USD |
|---|---|---|---|---|---|---|
| default (early exit at ≤ 10 bps) | 44 | 95.5% | 63.6% | +22.6 | +249 | 263 |
| `--no-early-exit` (8 h hold, like the research) | 21 | 76.2% | 57.1% | +78.0 | +410 | 142 |

- **Early exit:** exiting at convergence harvests the premium (about 113 bps of basis per trade) but gives up the directional drift that dominates the research "full" return. Whether early exit should stay on is a strategy decision. Set `strategy.early_exit_on_converge = false` to reproduce the research rule.
- **Hit-rate kill:** with `--no-early-exit`, the kill fired on 2026-11-10 (15/20 trades toward zero, below 80%). The replay measures the exit premium an hour earlier than the research does.

### Caveats (read these)

1. **About 17 days of history and 24 trades.** No statistical claim survives that sample size. The overlap-free trade count is small, and the market regime can change at any time.
2. **Pre-IPO oracle.** The oracle for a pre-IPO company is a constructed price, not a traded underlying. Several things can move the "fair value" the premium is measured against, or end the market: an IPO announcement, a funding round, an oracle methodology change, a settlement or a halt. There is no arbitrage leg against another venue: `io:ANTH` uses different share conventions.
3. **Growth-mode fee risk.** The backtest edge assumes growth-mode fees (0.6 bp round trip). The deployer can turn growth mode off (30-day cooldown per asset), and standard fees are 10x higher. The bot watches `growthMode`, `deployerFeeScale` and `lastFeeScaleChangeTime` and kills on any change. Update `[fees]` before resetting the kill.
4. **Part of the backtest PnL is directional.** The oracle moved in the trades' favour. The basis-only figure is the more honest one.
5. **Maker fills are optimistic** in both the research and the paper model. The research assumed fills at the hourly close, with no adverse selection.

## Architecture

```
src/
  main.rs            CLI (run, replay, probe, check-config, kill-status, reset-kill)
  config.rs          TOML config, validation, hard caps, mode endpoints
  mode.rs            mode guard (paper / testnet / guarded mainnet), credential loading, key redaction
  runner.rs          main loop: tokio::select! over market data, user events, timers, signals
  engine.rs          synchronous core shared by run and replay: strategy + risk per update
  replay.rs          historical replay / backtest
  runlock.rs         "bot is running" marker (heartbeat file in the data dir)
  hl/
    info.rs          REST info client with a token bucket (rate-limit aware)
    ws.rs            websocket feed: reconnect + backoff, REST fallback, staleness
    market.rs        market snapshot (book, ctx, premium)
    assets.rs        HIP-3 asset resolution (perpDexs + meta, asset id)
    rounding.rs      price tick / 5 significant figures / szDecimals
    signing.rs       L1 action signing (msgpack -> keccak -> phantom agent EIP-712)
    actions.rs       order / cancel / leverage actions (wire format)
    exchange.rs      /exchange client, refuses mainnet without the guard token or with a main-wallet key
    types.rs         API types
  strategy/
    premium.rs       premium computation and rolling mean
    signal.rs        entry / exit state machine
    sizing.rs        notional -> size, caps
  risk/
    kill.rs          kill rules, persisted risk state
    spec.rs          watched market spec (spec-change rule)
  exec/
    paper.rs         paper executor: maker fill model, IOC book walk, funding
    live.rs          testnet / mainnet executor: cloids, reconciliation, userFills
    state.rs         persisted executor state
  monitor/
    logging.rs       tracing to stdout + daily rolling file
    csv.rs           signals / orders / trades journals
    telegram.rs      rate-limited, non-blocking Telegram alerts
    commands.rs      read-only Telegram commands (/pnl, /trades, /help)
    alerts.rs        alert texts, daily summary
tests/               integration tests (fixtures from real API responses, signing vectors, mocks)
```

**Data flow.** The websocket/REST feed updates a shared `MarketState`. On every update the `Engine` builds a market view, updates the premium average, runs the risk rules and ticks the strategy. The strategy returns order commands, which the `Executor` (paper or live) carries out. Executor feedback (fills, done orders, funding) goes back to the strategy and to the ledger. The runner also persists state, writes the journals, polls `meta` for spec changes, reconciles live state, and handles SIGINT/SIGTERM.

**Signing.** Signing is implemented in-crate with `k256` + `sha3` + `rmp-serde`. It is tested against the official Python SDK vectors (`tests/signing_vectors.rs`), including HIP-3 asset ids, agent keys, vault addresses and `expiresAfter`.

**Paper fill model.** An ALO that would cross the book is rejected (post-only). A resting order fills at its own price when one of these happens:
- the opposite best price reaches it;
- a public trade prints through it;
- a public trade prints at it from the other side (`paper.fill_on_touch`).

Fills at the touch are reduced by a queue model: the size already resting at our price is ahead of us. IOC orders walk the book depth. Fees come from `[fees]`, and funding is settled hourly.

## Build and run

Requires Rust 1.85 or newer (`rust-version` in `Cargo.toml`; edition 2021). CI uses the latest stable.

```sh
cargo build --release
cargo test

./target/release/antbot check-config     # validate config/default.toml and the mode guard (no request is made)
./target/release/antbot probe --secs 30  # read-only: resolve xyz:ANT and print book / oracle / premium
./target/release/antbot run              # paper trading on live mainnet data (default)
./target/release/antbot kill-status      # show the persisted kill state
./target/release/antbot replay --help    # historical replay
```

Global option: `-c/--config <file>` (env `ANTBOT_CONFIG`, default `config/default.toml`). `run`, `check-config`, `kill-status` and `reset-kill` accept `--data-dir` (env `ANTBOT_DATA_DIR`) to override `monitor.data_dir`. `check-config` prints the effective data dir.

A `.env` file is loaded at startup (`cp .env.example .env`). It is searched in the working directory, then in its parent directories. It never overrides variables that are already set in the environment.

### Paper

```sh
cargo run --release -- run
```

Paper mode streams mainnet market data and simulates fills against it. It needs no key, has no exchange URL (`/exchange` cannot be called), and saves its account in `<data_dir>/state_paper.json`. Stop with Ctrl-C; a restart resumes the position and history.

### Testnet

`xyz:ANT` does **not** exist on testnet. The testnet `xyz` dex lists other assets, such as `xyz:TSLA`. To exercise the live executor and signing on testnet:

1. Create an API wallet on the testnet app (see [below](#hyperliquid-api--agent-wallet)) and fund the main testnet account. The testnet faucet requires the same address to have a mainnet deposit.
2. Write a separate config, for example `cp config/default.toml config/local.toml` (git-ignored). Set `mode = "testnet"` and `market.coin` to a testnet coin.
3. Read the coin's spec with `antbot -c config/local.toml probe --testnet`. Update the `market.expected_*` values from it, otherwise the spec-change rule kills at once.
4. Set `HL_PRIVATE_KEY` (and `HL_ACCOUNT_ADDRESS` for an API wallet) in `.env`.
5. `antbot -c config/local.toml check-config`, then `antbot -c config/local.toml run`.

Testnet uses `<data_dir>/state_testnet.json`. Use a separate `--data-dir` per mode and per coin.

## Modes and the mainnet guard

| mode | market data | orders | key |
|---|---|---|---|
| `paper` (default) | mainnet info + ws | simulated locally, `/exchange` never called | not read |
| `testnet` | testnet info + ws | signed, sent to the testnet `/exchange` | required |
| `mainnet` | mainnet info + ws | signed, sent to the mainnet `/exchange`: **REAL FUNDS**, only when every guard below passes | API wallet key + main account required |

The mode comes from `mode` in the config or from `--mode`. Mainnet requires **all** of these, otherwise `run` exits non-zero with an explicit error **before** logging starts and before any network or exchange client is built:

1. `--mode mainnet` (or `mode = "mainnet"`) **and** the `--mainnet` CLI flag;
2. `ANTBOT_CONFIRM_MAINNET=yes-i-understand` in the environment (exact value);
3. `HL_PRIVATE_KEY` set to a valid key: the **API (agent) wallet** key;
4. `HL_ACCOUNT_ADDRESS` set to a valid address: the **main** account. It is required on mainnet because an API wallet holds no funds: positions, margin, orders and fills are queried for the main account;
5. the address derived from `HL_PRIVATE_KEY` must **differ** from `HL_ACCOUNT_ADDRESS`. If they are equal, the key is the main wallet's own key, which can also withdraw funds. Mainnet refuses it and asks for an API wallet (Hyperliquid UI: More > API), which can trade but cannot withdraw. Testnet still accepts a main wallet key.

Passing `--mainnet` when the resolved mode is not mainnet is an error. Defence in depth: the exchange client only accepts a mainnet URL with a confirmation token that only the guard can produce, refuses to sign for mainnet when the signer is the traded account (main-wallet key), and testnet mode can never reach the mainnet URL.

`check-config` (no request is made) evaluates the same rules plus the exchange URL guard. In mainnet mode it prints every guard condition as `[ok]` / `[FAIL]` (so you see everything that is missing at once), then `mainnet guard: PASSED` or `REFUSED`, and exits non-zero when any guard refuses.

At startup on mainnet the log shows a `MODE: MAINNET - REAL FUNDS` banner with the main account address and the signer (API wallet) address, never the key. Telegram alerts are prefixed `[antbot MAINNET <coin>]` and the startup alert starts with `🚨 MODE: MAINNET - REAL FUNDS` followed by both addresses.

### Safety checks on the live path (testnet and mainnet)

The same rules apply on testnet and mainnet:

- **Notional caps.** Sizing never exceeds `sizing.max_notional_usd` nor the built-in 10,000 USD hard cap (config validation, sizing, and a strategy check on committed notional). The live executor re-checks every order that can open or grow a position just before signing it. It reads `clearinghouseState` first. If that read fails, the order is refused: a stale snapshot is never used to admit new exposure. After 3 failed reads in a row, reconciliation sends an alert, and another one when reads work again. The executor then requires that the following stay within `max_notional_usd` (+2% tolerance for an entry re-quoted after the touch moved) and never above the hard cap:
  - the order notional;
  - the venue position (at its entry price);
  - any other active opening order (for example one whose outcome is uncertain);
  - the unconfirmed part of recently finished opening orders. That is size reported filled (or a filled size that could not be read, counted as fully filled) whose fills have not arrived yet. It counts until the fills arrive, or until an account read taken 15 s after the order finished, which then includes them in the venue position.

  Violations are refused locally, logged and alerted; nothing is sent. While the account read fails, the bot does not compare a stale venue position with its own, so the position-mismatch check is suspended rather than satisfied by stale data.
- **Post-only entries.** Entries are ALO (post-only) limit orders only; the executor refuses any non-reduce-only order that is not ALO. Exits are reduce-only: maker ALO first, then a reduce-only IOC taker fallback near the max-hold deadline or `exit_maker_timeout_secs` into a convergence exit. The kill flatten is reduce-only ALO for `kill_flatten_maker_secs`, then reduce-only IOC. Reduce-only orders are never blocked by the caps, since they can only shrink the position.
- **Isolated margin and leverage.** Only `margin_mode = "isolated"` is accepted, and leverage is clamped to `1..=min(2, asset max)`. The bot sends `updateLeverage` with `isCross = false` at startup when flat, and again right before every opening order placed while the venue position is flat. That order goes out only if the call succeeded; a failure refuses it. No earlier success is reused: `clearinghouseState` shows no margin settings while flat, so a manual switch to cross margin or higher leverage is corrected before the next entry instead of applying to it. Before adding to an existing position, the executor checks the margin settings that `clearinghouseState` reports for it. It refuses the order unless the position is isolated at no more than the bot's leverage. A position found open at startup keeps its settings, since they cannot be changed while it is open. If it is on cross margin or above the bot's leverage, an alert says so and the position is exit-only: the bot cancels its own active opening orders (for example a partially filled entry restored from the previous run and still resting), at startup and again on every reconciliation (`live.reconcile_secs`) that still sees those settings, and a failed cancel is retried. Reduce-only exits still go out, but nothing may grow the position. The same applies when a running position is switched to unsafe settings on the exchange: the next reconciliation cancels the opening orders. Once it is closed, isolated leverage is set again before the next entry. Remaining gaps: the settings are only re-read at those points, so a change made on the exchange takes up to one reconciliation period to be acted on; and `updateLeverage` and the order are two separate actions, so a manual change in the few milliseconds between them is not caught. Do not change the asset's margin settings by hand while the bot runs.
- **Kill switches.** Hit rate, daily loss, max drawdown, persistent premium and spec change (plus position mismatch) are evaluated by the engine shared by paper and live. On a kill, new entries stop, open orders are canceled and, with `risk.flatten_on_kill = true` (default), the position is flattened as above. Kills are persisted and survive restarts until `reset-kill --confirm`.
- **The key is never written out.** Once loaded it is held in a type with no `Display` and a redacting `Debug`, wiped when dropped (transient copies made while parsing are not all wiped, and the variable stays in the process environment, readable by root via `/proc/<pid>/environ`). No error message contains it. Fatal errors are printed with long hex runs scrubbed, so a config path that mistakenly points at the env file does not echo the key. Logs, alerts, journals and the persisted state hold addresses only.

## Environment variables

| variable | used in | meaning |
|---|---|---|
| `HL_PRIVATE_KEY` | testnet / mainnet | signing key, 64 hex, with or without `0x`. **Use an API/agent wallet key, not your main wallet key** (mainnet refuses a main wallet key). Never read in paper mode, never logged |
| `HL_ACCOUNT_ADDRESS` | testnet / mainnet | main account address (`0x` + 40 hex) when `HL_PRIVATE_KEY` is an API wallet. Positions, balances, open orders and fills are queried for this address. **Required on mainnet.** On testnet, empty = the address derived from the key |
| `ANTBOT_CONFIRM_MAINNET` | mainnet | must equal `yes-i-understand` (plus `--mode mainnet --mainnet`). Leave empty except in the mainnet env file |
| `TELEGRAM_BOT_TOKEN` | all | Telegram bot token. Alerts are disabled if unset |
| `TELEGRAM_CHAT_ID` | all | chat id that receives the alerts. Commands (`/pnl`, `/trades`, `/help`) are answered only when this is your own user id (private chat with the bot), see [Telegram commands](#telegram-commands) |
| `TELEGRAM_API_BASE` | all | optional Bot API base URL (default `https://api.telegram.org`, used by tests) |
| `RUST_LOG` | all | log filter (tracing `EnvFilter` syntax). Default and `.env.example`: `info`. `info,antbot=debug` adds the bot's debug lines. An invalid value falls back to `info` with a warning in the log |
| `ANTBOT_CONFIG` | all | config file path (same as `-c`) |
| `ANTBOT_DATA_DIR` | `run`, `check-config`, `kill-status`, `reset-kill` | data dir override (same as `--data-dir`). The Docker image sets `/data` |
| `ANTBOT_UID` / `ANTBOT_GID` | `docker compose` only | uid / gid the container runs as (default `10001`). It must own `./data`. Not read by the bot |

`.env.example` lists them. `.env` is git-ignored and must never be committed.

## Hyperliquid API / agent wallet

Hyperliquid lets a main account authorize **API wallets** (also called agent wallets). An API wallet is a separate key that can sign trading actions (orders, cancels, leverage) for the main account but **cannot withdraw or transfer funds**. A leaked bot key then risks bad trades but not a withdrawal. Use one for this bot.

To create one:

1. Open the Hyperliquid app ([app.hyperliquid.xyz/API](https://app.hyperliquid.xyz/API), or [app.hyperliquid-testnet.xyz/API](https://app.hyperliquid-testnet.xyz/API) for testnet), connected with your **main** wallet.
2. Enter a name for the API wallet and click "Generate". Copy the **private key**: it is only shown once.
3. Click "Authorize API Wallet", choose a validity period, and sign the approval with the main wallet.
4. In `.env`:
   - `HL_PRIVATE_KEY` = the API wallet private key;
   - `HL_ACCOUNT_ADDRESS` = the **main** account address (not the API wallet address).

Notes:
- `HL_ACCOUNT_ADDRESS` is required with an API wallet (and always on mainnet). The API wallet holds no funds or positions; the info queries (`clearinghouseState`, `openOrders`, `userFills`, ws `orderUpdates`/`userFills`) must target the main account.
- Hyperliquid tracks nonces per signer. Use **one API wallet per running bot**, and do not share it with other software.
- API wallets expire, and unused ones can be pruned. Renew before expiry and deregister old ones.
- `vaultAddress` is not used: it is for vaults and sub-accounts, not agent wallets.
- HIP-3 dexs have their own clearinghouse. Make sure the account has collateral usable on the `xyz` dex. The bot reads `clearinghouseState` with `dex: "xyz"` for positions and available margin.
- `antbot check-config --mode testnet` prints the signer and account addresses without making any request.

## Running on mainnet (real funds)

**This trades real money.** Run paper (and ideally testnet) first, read [Risks and limitations](#risks-and-limitations), and start small. Never put the key in the TOML config or on the command line (it would end up in shell history and `ps`).

### 1. Create and authorize an API wallet

1. Open [app.hyperliquid.xyz](https://app.hyperliquid.xyz) connected with your **main** wallet, then **More > API** ([app.hyperliquid.xyz/API](https://app.hyperliquid.xyz/API)).
2. Name the API wallet and click "Generate". Copy its **private key** (shown once).
3. Click "Authorize API Wallet", choose a validity period, and sign with the main wallet. The API wallet can now trade for the main account but **cannot withdraw or transfer**.
4. Note the **main** account address (the wallet you are connected with, not the API wallet address).

Use this API wallet for this bot only (nonces are tracked per signer). Renew it before it expires.

### 2. Deposit margin on the `xyz` dex

`xyz:ANT` is a HIP-3 perp: the `xyz` dex has its own clearinghouse and USDC collateral that is separate from the main perps balance. Move the USDC you want to trade with to the `xyz` dex in the Hyperliquid UI (open the `xyz:ANT` market and transfer USDC to that dex). The bot reads `clearinghouseState` with `dex: "xyz"` for the account's margin and positions. With no margin there the available margin is 0 and the bot cannot size an entry. To check from the VPS (read-only info call, replace the address):

```sh
curl -s https://api.hyperliquid.xyz/info -H 'Content-Type: application/json' \
  -d '{"type":"clearinghouseState","user":"0xYOUR_MAIN_ACCOUNT","dex":"xyz"}' | jq '.marginSummary, .withdrawable'
```

### 3. Separate directory, env file, config and binary

The live instance gets its own directory: never reuse a paper data dir (`risk_state.json` and the state files must not mix).

```sh
mkdir -p /root/projects/antbot-live/bin /root/projects/antbot-live/data
chmod 700 /root/projects/antbot-live

# A copy of the binary, so rebuilding the repo never swaps it under the running bot
cd /root/projects/antbot && cargo build --release
cp target/release/antbot /root/projects/antbot-live/bin/antbot

# Its own config
cp config/default.toml /root/projects/antbot-live/live.toml
```

Edit `/root/projects/antbot-live/live.toml`:

```toml
mode = "mainnet"

[sizing]
notional_usd = 250.0          # first run: a few hundred USD (exchange minimum: 10 USD per order)
max_notional_usd = 300.0      # opening orders above this (+2% re-quote tolerance) are refused

[monitor]
data_dir = "/root/projects/antbot-live/data"
# If another instance (e.g. paper) already polls the same Telegram bot token,
# disable commands here or use a separate bot (two pollers get 409 Conflict).
# telegram_commands_enabled = false
```

Keep the `[risk]` defaults (or tighten `daily_loss_limit_usd` / `max_drawdown_usd` for the first run); the `[market] expected_*` values must match the live spec (`antbot probe`), or the spec-change rule kills at once.

Create the env file `/root/projects/antbot-live/live.env` with exactly these variable names, and restrict it:

```sh
cat > /root/projects/antbot-live/live.env <<'EOF'
HL_PRIVATE_KEY=0x...                      # the API wallet private key (64 hex)
HL_ACCOUNT_ADDRESS=0x...                  # the MAIN account address (40 hex)
ANTBOT_CONFIRM_MAINNET=yes-i-understand
# Optional Telegram alerts (and /pnl, /trades, /help commands)
TELEGRAM_BOT_TOKEN=
TELEGRAM_CHAT_ID=
EOF
chmod 600 /root/projects/antbot-live/live.env
```

The bot reads the **process environment**. It does not read `live.env` by itself (it only auto-loads a file named `.env` from the working directory or its parents, which never overrides variables already set). Export the file into the bot's environment with `set -a; . live.env; set +a`, for example via a small launcher, `/root/projects/antbot-live/run-live.sh`:

```sh
#!/usr/bin/env bash
# Launch antbot on mainnet with the variables from live.env (never on the command line).
set -euo pipefail
cd /root/projects/antbot-live
set -a; . ./live.env; set +a
exec ./bin/antbot -c ./live.toml run --mode mainnet --mainnet --data-dir ./data
```

```sh
chmod 700 /root/projects/antbot-live/run-live.sh
```

### 4. Check, then launch

`check-config` makes no request. It prints every mainnet guard condition and exits non-zero if `run` would refuse:

```sh
cd /root/projects/antbot-live
( set -a; . ./live.env; set +a; ./bin/antbot -c ./live.toml check-config --mode mainnet --mainnet --data-dir ./data )
```

Expect five `[ok]` lines, `mainnet guard: PASSED`, the signer (API wallet) and main account addresses, `exchange url: https://api.hyperliquid.xyz/exchange` and your reduced sizing. Then launch (in the background, logs to a file; the bot also writes `data/logs/antbot.log.YYYY-MM-DD`):

```sh
cd /root/projects/antbot-live
nohup ./run-live.sh >> run-live.out 2>&1 &
tail -f run-live.out          # look for "MODE: MAINNET - REAL FUNDS", "asset resolved", "LIVE executor", "main loop started"
```

A `tmux` / `screen` session running `./run-live.sh` works as well.

### 5. Stop, flatten, reset a kill

- **Status:** `/pnl` and `/trades` on Telegram; the last heartbeat (`grep heartbeat /root/projects/antbot-live/data/logs/antbot.log.$(date -u +%F) | tail -1`: phase, position, venue position, equity, kill); `./bin/antbot -c ./live.toml kill-status --data-dir ./data`; and `data/trades.csv` / `data/orders.csv`. (`scripts/paper_status.sh` is for paper data dirs.)
- **Stop:** `kill -TERM "$(jq -r .pid /root/projects/antbot-live/data/antbot.lock)"` (or Ctrl-C in the foreground). The bot cancels its open orders, saves its state and **keeps the position** (it resumes managing it at the next start).
- **Flatten:**
  - automatically on any kill (`risk.flatten_on_kill = true`): reduce-only maker first for `kill_flatten_maker_secs`, then reduce-only IOC;
  - on stop, if `[live] flatten_on_shutdown = true` was set in `live.toml` **before** starting: a reduce-only IOC within `strategy.taker_max_slippage_bps` of the touch;
  - or manually in the Hyperliquid UI after stopping the bot. At the next start the bot replays the fills it missed: your closing fill is booked as an external close (alert "fill for an order the bot did not place"), the trade is recorded with exit reason `external`, and the bot is flat again. **It then resumes trading after `cooldown_after_exit_secs`.** To stop trading, keep the bot stopped. If its saved position and the exchange still disagree after the replay, it halts without trading (a kill): check with `kill-status`, then `reset-kill --confirm`.
- **Reset a kill** (after reviewing the reason; the bot must be stopped, `reset-kill` refuses otherwise):

  ```sh
  cd /root/projects/antbot-live
  ./bin/antbot -c ./live.toml kill-status --data-dir ./data
  ./bin/antbot -c ./live.toml reset-kill --confirm --data-dir ./data
  nohup ./run-live.sh >> run-live.out 2>&1 &
  ```

  `kill-status` and `reset-kill` never read the key.
- **Scale up** only after a meaningful live track record: raise `notional_usd` / `max_notional_usd` in `live.toml` (hard cap 10,000 USD) and restart.
- **Revoke** the API wallet in the Hyperliquid UI (More > API) when you stop using the bot, or if the VPS or the env file may have been exposed.

## Configuration reference

All thresholds live in `config/default.toml`, which is commented key by key. Unknown keys are rejected, and missing keys take the defaults below. The config is validated as a whole at load time: every violation is reported (see [Validation](#validation)). Amounts are USD, durations seconds, fee rates fractions (0.0001 = 1 bp) unless the key ends in `_bps`/`_ms`.

### Top level

| key | default | meaning |
|---|---|---|
| `mode` | `"paper"` | `paper` \| `testnet` \| `mainnet` (see the guard above). `--mode` overrides it |

### `[market]`

| key | default | meaning |
|---|---|---|
| `dex` | `"xyz"` | HIP-3 builder dex name |
| `coin` | `"xyz:ANT"` | coin name, must be `<dex>:<name>` |
| `expected_funding_multiplier` | `0.005` | baseline `perpDexs.assetToFundingMultiplier` |
| `expected_growth_mode` | `"enabled"` | baseline `meta.universe[].growthMode` |
| `expected_deployer_fee_scale` | `1.0` | baseline `deployerFeeScale` |
| `expected_sz_decimals` | `2` | baseline `szDecimals` (also used by replay) |
| `expected_max_leverage` | `10` | baseline `maxLeverage` (also used by replay) |

At the first start, the live spec is checked against these baseline values and then persisted. Any later difference triggers the spec-change kill (see [Kill rules](#kill-rules)).

### `[network]`

| key | default | meaning |
|---|---|---|
| `mainnet_info_url` | `https://api.hyperliquid.xyz/info` | mainnet REST info (paper uses it) |
| `mainnet_exchange_url` | `https://api.hyperliquid.xyz/exchange` | mainnet exchange (never used by paper or testnet) |
| `mainnet_ws_url` | `wss://api.hyperliquid.xyz/ws` | mainnet websocket |
| `testnet_info_url` | `https://api.hyperliquid-testnet.xyz/info` | testnet REST info |
| `testnet_exchange_url` | `https://api.hyperliquid-testnet.xyz/exchange` | testnet exchange, must differ from mainnet |
| `testnet_ws_url` | `wss://api.hyperliquid-testnet.xyz/ws` | testnet websocket |
| `http_timeout_secs` | `10` | HTTP timeout |

### `[strategy]`

| key | default | meaning |
|---|---|---|
| `premium_source` | `"impact"` | `impact` (impact mid vs oracle, as backtested) \| `book_mid` (l2Book mid vs oracle) |
| `entry_threshold_bps` | `50.0` | enter when \|signal\| ≥ this |
| `exit_converge_bps` | `10.0` | early exit when the signal is back to ≤ this on the faded side (or crossed). Must be < entry |
| `early_exit_on_converge` | `true` | `false` = pure max-hold exit (the research `8h_fixed` rule) |
| `max_hold_secs` | `28800` | 8 h hard deadline from the first fill |
| `cooldown_after_exit_secs` | `3600` | no new entry for this long after an exit |
| `entry_timeout_secs` | `900` | give up an unfilled entry after this long |
| `requote_interval_secs` | `20` | minimum delay between re-prices of a resting ALO |
| `requote_drift_bps` | `5.0` | re-price only if our price is this far from the target |
| `taker_fallback_window_secs` | `900` | IOC taker fallback inside this window before max hold |
| `exit_maker_timeout_secs` | `600` | two uses: (1) a convergence exit goes taker after this long resting; (2) the max-hold maker exit starts at deadline − `taker_fallback_window_secs` − this (7 h 35 min by default) |
| `taker_max_slippage_bps` | `50.0` | IOC limit = best ± this (max 500) |
| `premium_avg_secs` | `3600` | rolling mean window of the signal (5 s slots). `0` = instantaneous premium (not the backtested signal). Max 14400 |
| `entry_retry_cooldown_secs` | `60` | wait after an entry abandoned with nothing filled |
| `min_order_interval_ms` | `1000` | minimum gap between two new orders |
| `max_order_actions_per_min` | `30` | cap on new orders + cancels per rolling minute (cancels are never blocked, only counted) |

### `[sizing]`

| key | default | meaning |
|---|---|---|
| `notional_usd` | `2500.0` | notional per position |
| `max_notional_usd` | `10000.0` | cap; cannot exceed the built-in **10,000 USD hard cap** |
| `leverage` | `1` | isolated leverage, 1 or 2 (built-in maximum 2) |
| `margin_mode` | `"isolated"` | only `isolated` is accepted |

### `[risk]`

| key | default | meaning |
|---|---|---|
| `hit_rate_window` | `20` | rolling window of closed trades |
| `hit_rate_min` | `0.80` | kill when the hit rate over the window falls below this |
| `hit_definition` | `"premium_toward_zero"` | a hit is \|premium\| closer to 0 at exit than at entry; or `pnl_positive` (net PnL > 0) |
| `persistent_premium_bps` | `150.0` | kill when \|signal\| stays above this… |
| `persistent_premium_secs` | `7200` | …for this long (regime change) |
| `kill_on_spec_change` | `true` | kill on a market spec change |
| `daily_loss_limit_usd` | `150.0` | kill when the UTC-day PnL (realized + unrealized) is ≤ −this |
| `max_drawdown_usd` | `300.0` | kill when equity is this far or more below its peak (≥) |
| `flatten_on_kill` | `true` | close the position on kill (maker, then taker). `false` = cancel orders and keep the position |
| `kill_flatten_maker_secs` | `120` | maker phase of a kill flatten before IOC (0 = taker at once) |

### `[fees]`

| key | default | meaning |
|---|---|---|
| `maker_rate` | `0.00003` | 0.003%: tier 0, HIP-3 scale 1.0, growth mode on. Standard would be 0.00030 |
| `taker_rate` | `0.00009` | 0.009%. Standard would be 0.00090 |

These rates drive paper fills, replay and PnL accounting. The venue charges its own fees in live modes.

### `[timing]`

| key | default | meaning |
|---|---|---|
| `rest_fallback_poll_secs` | `5` | REST `metaAndAssetCtxs` + `l2Book` poll period while the ws is stale |
| `ws_stale_secs` | `15` | no ws market data for this long → stale (REST fallback, no new entries on stale data) |
| `ws_ping_secs` | `50` | ws ping interval |
| `ws_reconnect_initial_ms` | `500` | initial reconnect backoff |
| `ws_reconnect_max_ms` | `30000` | maximum reconnect backoff |
| `meta_poll_secs` | `60` | `perpDexs` + `meta` poll for spec changes |
| `signal_sample_secs` | `60` | one `signals.csv` row per period |
| `state_persist_secs` | `30` | state save period (also saved on important events) |
| `heartbeat_secs` | `300` | heartbeat log line period |
| `rest_bucket_capacity` | `100.0` | REST token bucket size (Hyperliquid allows 1200 weight/min per IP) |
| `rest_refill_per_sec` | `10.0` | REST token bucket refill rate |

### `[monitor]`

| key | default | meaning |
|---|---|---|
| `data_dir` | `"data"` | state, CSV journals, `logs/` (overridden by `--data-dir` / `ANTBOT_DATA_DIR`) |
| `log_json` | `false` | JSON lines instead of text on both outputs |
| `log_max_files` | `30` | daily log files kept (0 = keep all). `1` acts as `2` (today and the previous file). At startup the newest files are kept, today's included, and a same-day restart neither erases today's log nor the previous one. A process started on a day whose log file already existed keeps one extra old file after its day rollovers, until it is restarted |
| `telegram_enabled` | `true` | Telegram alerts (still need the env vars) |
| `telegram_max_per_min` | `20` | Telegram rate limit (1–30); queued alerts are merged |
| `telegram_commands_enabled` | `true` | answer the read-only `/pnl`, `/trades`, `/help` commands. Active only with `telegram_enabled`, both env vars set, and `TELEGRAM_CHAT_ID` a private chat (your user id) |
| `daily_summary` | `true` | send a summary of the previous UTC day after 00:00 UTC (also at the next start if it was missed) |

### `[paper]`

| key | default | meaning |
|---|---|---|
| `initial_balance_usd` | `10000.0` | simulated account balance (available margin) |
| `fill_on_touch` | `true` | a trade printing *at* our price (other side aggressor) fills us. `false` = only trades strictly through our price |
| `queue_model` | `true` | the size already resting at our price when we join is ahead of us. `false` = we are first in the queue (optimistic) |

### `[live]` (testnet / mainnet)

| key | default | meaning |
|---|---|---|
| `stray_orders` | `"cancel"` | unknown open orders on the coin at startup: `cancel` \| `keep` |
| `adopt_position` | `true` | manage a position found at startup (fresh 8 h clock). `false` = halt |
| `cancel_on_shutdown` | `true` | SIGINT/SIGTERM cancels the bot's open orders |
| `flatten_on_shutdown` | `false` | close the position on shutdown (default: keep it) |
| `reconcile_secs` | `30` | `clearinghouseState` / `openOrders` reconciliation period |
| `max_exchange_actions_per_min` | `60` | client-side per-address action budget (cancels always allowed; the `updateLeverage` sent before an entry from flat is counted but never blocks that entry) |
| `rate_limit_backoff_secs` | `60` | pause new orders after a rate-limit answer |
| `expires_after_ms` | `30000` | `expiresAfter` on signed actions (0 = off) |

### Validation

Besides the types and the allowed values in the tables above, `validate` checks:

- **Cross-field:**
  - `market.coin` starts with `<market.dex>:`;
  - `strategy.exit_converge_bps` < `strategy.entry_threshold_bps` < `risk.persistent_premium_bps`;
  - `strategy.taker_fallback_window_secs` and `risk.kill_flatten_maker_secs` are each < `strategy.max_hold_secs`;
  - `sizing.notional_usd` ≤ `sizing.max_notional_usd` ≤ 10,000 (hard cap);
  - `sizing.leverage` ≤ `market.expected_max_leverage`;
  - `fees.taker_rate` ≥ `fees.maker_rate`;
  - `timing.ws_reconnect_max_ms` ≥ `timing.ws_reconnect_initial_ms`;
  - `network.testnet_exchange_url` ≠ `network.mainnet_exchange_url`.
- **Ranges:**
  - `market`: `dex` and `coin` not empty, `expected_funding_multiplier` and `expected_deployer_fee_scale` ≥ 0, `expected_sz_decimals` ≤ 6, `expected_max_leverage` ≥ 1;
  - `network`: info/exchange URLs start with `https://`, ws URLs with `wss://`; `http_timeout_secs` > 0;
  - `strategy`: `entry_threshold_bps` > 0; `exit_converge_bps` and `requote_drift_bps` ≥ 0; `max_hold_secs`, `entry_timeout_secs`, `requote_interval_secs` and `exit_maker_timeout_secs` > 0; `taker_max_slippage_bps` in (0, 500]; `premium_avg_secs` ≤ 14400; `min_order_interval_ms` ≤ 60000; `max_order_actions_per_min` in 1..=600;
  - `sizing`: `notional_usd` and `max_notional_usd` > 0, `leverage` in 1..=2, `margin_mode = "isolated"`;
  - `risk`: `hit_rate_window` ≥ 1, `hit_rate_min` in (0, 1], `persistent_premium_bps`, `persistent_premium_secs`, `daily_loss_limit_usd` and `max_drawdown_usd` > 0;
  - `fees`: each rate within [−0.01, 0.01] (a negative maker rate, i.e. a rebate, is allowed);
  - `timing`: every `*_secs` key and `ws_reconnect_initial_ms` > 0, `ws_ping_secs` < 60, `rest_bucket_capacity` > 0, `rest_refill_per_sec` in (0, 20];
  - `paper.initial_balance_usd` > 0;
  - `live`: `reconcile_secs` in 1..=3600, `max_exchange_actions_per_min` ≥ 1, `expires_after_ms` 0 or in 5000..=300000;
  - `monitor`: `data_dir` not empty, `telegram_max_per_min` in 1..=30.

## Replay / backtest

`antbot replay` runs the same `Engine` (strategy + risk) and paper fill model as `run`, over hourly history. It is read-only and never trades.

```sh
# Fetch 30 days from the mainnet info API (read-only) and save it
antbot replay --fetch --days 30 --save-dir replay-data

# Re-run on saved files, compare with the research results, export trades
antbot replay --input replay-data --compare path/to/results.json --trades-csv replay-trades.csv

# Research rule: no early exit, pure 8 h hold
antbot replay --input replay-data --no-early-exit
```

| option | meaning |
|---|---|
| `--input DIR` | directory with `candles_1h.json` and `funding_history.json` (or `api_funding_history.json`) |
| `--candles F --funding F` | explicit files |
| `--fetch [--days N] [--save-dir DIR]` | `candleSnapshot` + `fundingHistory` from the mainnet info API |
| `--compare F` | research `results.json`, S2_ANT `8h_fixed` growth maker/maker |
| `--no-early-exit` | disable the convergence exit |
| `--spread-bps X` | synthetic book spread (default 2) |
| `--step-secs N` | engine tick inside each hour (default 60) |
| `--trades-csv F` | write the trades with the `trades.csv` columns |

The historical premium is hourly (`fundingHistory.premium`). The replay reconstructs the oracle as `close / (1 + premium)` and builds a synthetic book around the close. It replays each hour's low and high as public trades, so a maker order fills when the hour's range reaches its price. The intra-hour premium path and the order of the low and high are unknown. Treat the replay as a sanity check of the wiring and fill model, not a precise backtest.

## Docker deployment on a VPS

The `Dockerfile` is a multi-stage build:
- **Builder:** Rust on Debian trixie, with BuildKit cache mounts for the cargo registry and target dir.
- **Runtime:** `debian:trixie-slim` with CA certificates only. It runs as the non-root user `antbot` (uid/gid 10001), with `/data` as a volume and a healthcheck on the run-lock heartbeat (`$ANTBOT_DATA_DIR/antbot.lock`, so it follows a data dir override).

`docker-compose.yml` needs **Docker Compose ≥ 2.24** (it uses `env_file` with `required: false`; older versions reject the file). It runs paper mode by default. It reads `env_file: .env`, mounts `./data:/data` and `./config:/app/config:ro`, uses `restart: unless-stopped`, and rotates the json-file logs (10 MB × 5). It allows 30 s on stop for the graceful shutdown.

```sh
# On the VPS (Docker Engine + compose plugin >= 2.24 installed)
git clone <your fork or private repo> antbot && cd antbot
cp .env.example .env && chmod 600 .env      # fill TELEGRAM_*; HL_* only for testnet
mkdir -p data && sudo chown 10001:10001 data # or set ANTBOT_UID/ANTBOT_GID in .env to your uid/gid
docker compose up -d --build
docker compose logs -f antbot               # look for "websocket connected" and "heartbeat"
docker compose ps                           # health: healthy once the run lock is fresh
```

Operations:

```sh
docker compose restart antbot                     # restart (state resumes from ./data)
docker compose down                               # stop (SIGTERM: cancel orders, save, flush alerts)
git pull && docker compose up -d --build          # update
docker compose run --rm antbot kill-status        # kill state
docker compose stop antbot && docker compose run --rm antbot reset-kill --confirm && docker compose start antbot
docker compose run --rm antbot check-config       # also prints the effective data dir
tail -f data/logs/antbot.log.$(date -u +%F)       # file logs on the host
```

- **Config:** edit `config/default.toml` on the host and restart. No rebuild is needed: `./config` is mounted. Alternatively, put a `config/local.toml` and set `ANTBOT_CONFIG=/app/config/local.toml` in `.env`.
- **Testnet:** never reuse the paper `/data`. `risk_state.json` (spec baseline, hit-rate window, equity peak) is shared between modes, and a testnet coin would fire the spec-change kill at once. Use a local config with `mode = "testnet"` and a separate data dir, in `.env`:
  ```sh
  ANTBOT_CONFIG=/app/config/local.toml
  ANTBOT_DATA_DIR=/data/testnet        # ./data/testnet on the host, created at startup
  HL_PRIVATE_KEY=...                   # plus HL_ACCOUNT_ADDRESS for an API wallet
  ```
  `docker compose up -d`, `run --rm antbot kill-status` / `reset-kill` / `check-config` and the healthcheck then all use `/data/testnet`. Remove both lines to return to paper on `/data`.
- **Clock:** the VPS clock must be NTP-synced. Signed actions carry millisecond nonces, and the daily rules use UTC days.

## Monitoring

Everything is written to the data dir (`monitor.data_dir`, `--data-dir` or `ANTBOT_DATA_DIR`; `/data` in Docker, `./data` on the host):

| file | content |
|---|---|
| `logs/antbot.log.YYYY-MM-DD` | daily rolling log, with the same lines as stdout. `monitor.log_max_files` files are kept. `RUST_LOG` sets the filter (an invalid value falls back to `info` with a warning), and `monitor.log_json = true` switches both outputs to JSON lines |
| `signals.csv` | one premium sample every `timing.signal_sample_secs`: `ts_ms, ts_utc, oracle_px, mark_px, impact_bid_px, impact_ask_px, impact_mid_px, best_bid, best_ask, book_mid_px, premium_bps` (the signal, i.e. the rolling mean), `raw_premium_bps, api_premium_bps, avg_ready, stale, state, position, killed` |
| `orders.csv` | every order command and its feedback: `ts_ms, ts_utc, mode, event (place / cancel / resting / fill / done), tag, side, kind, purpose, reduce_only, px, sz, fee_usd, crossed, status, filled_sz, detail`. On `place` rows `detail` is the touch the order was priced on (`book <best_bid>/<best_ask>`), so post-only and IOC prices can be checked against the book; on `done` rows it holds the reject reason |
| `trades.csv` | one row per round trip: `entry_ts_ms, entry_ts_utc, exit_ts_ms, exit_ts_utc, mode, side, size, entry_px, exit_px, notional_usd, fees_usd, funding_usd, gross_pnl_usd, pnl_usd, pnl_bps, reason, entry_premium_bps, exit_premium_bps, hold_secs, exit_taker_sz, signal_ts_ms` |
| `state_<mode>.json` | persisted strategy, ledger (equity curve, trade history), executor state and the daily summary tracker |
| `risk_state.json` | kill state, hit-rate window, equity peak, day start, spec baseline |
| `antbot.lock` | run marker with a heartbeat. `reset-kill` refuses to run while it is fresh. The Docker healthcheck uses it |

- **CSV journals:** headers are written once and restarts append. If a newer version changes the columns, the old file is renamed `<name>.<unix_ms>.old` and a new one starts. Rows are written synchronously on the main loop and flushed to the OS, not fsynced: they survive a process crash but not necessarily a power loss. After a failed or partial write (or a crash mid-row), the next row starts on a new line; a row cut inside a quoted field has its quote closed first, so it cannot swallow the rows after it.
- **Heartbeat:** a `heartbeat` log line every `timing.heartbeat_secs` with `phase`, `premium_bps` (the signal), `raw_premium_bps`, `position`, `unrealized_usd`, `working_order`, `equity_usd`, `realized_usd`, `trades`, `venue_position`, `killed`, and feed health: `ws_connected`, `ws_connects`, `rest_polls` and `data_age_ms` (age of the older of the last book and the last asset ctx).
- **Healthy run:** at startup you should see `antbot starting`, `asset resolved`, `websocket connected` and `main loop started`, then heartbeats.
- **Quick status:** `scripts/paper_status.sh <data_dir>` (or an instance dir containing `data/`) is a read-only summary of a paper run: process alive (from `antbot.lock`), last heartbeat line, latest premium sample and the range seen in `signals.csv`, phase and position, kill state, order and trade counts, and cumulative paper PnL from `trades.csv`. It needs `jq` and `awk` and never touches the network.

### Telegram setup

1. In Telegram, talk to [@BotFather](https://t.me/BotFather), send `/newbot` and copy the **bot token**.
2. Send any message to your new bot (or add it to a group and send a message there).
3. Get the chat id: open `https://api.telegram.org/bot<TOKEN>/getUpdates` and read `result[].message.chat.id`. Group ids are negative.
4. Set `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` in `.env` and restart. For the [commands](#telegram-commands), use your private chat with the bot: its chat id is your user id, a positive number.

Without them, alerts are disabled with a single log line.

Alerts are prefixed `[antbot <mode> <coin>]`. They cover:
- startup and shutdown;
- position opened (entry filled), exit started, taker fallback, and trade closed with PnL;
- the kill switch and market spec changes;
- warnings and errors: resyncs, mismatches, rate limits, state save failures, fatal errors, and strategy warnings (fill for an unknown order, closing fill larger than the open size, entry refused by the hard cap, giving up waiting for fills, entry sizing failures, stale data past the hold deadline). A strategy warning is always logged, and sent to Telegram at most once per 15 min for the same kind of message (numbers ignored);
- a daily summary of the previous UTC day (`monitor.daily_summary`).

Entry signals and aborted entries are log-only, because they can repeat every minute around the threshold.

The daily summary tracker (current UTC day, premium min / max / last) is persisted in `state_<mode>.json`:
- a summary missed while the bot was down over midnight UTC is sent at the next start. It covers the last day the bot was running, and its premium range covers only the time the bot was running;
- a restart in the middle of the day keeps the day's premium range.

Sending never blocks the trading loop:
- Alerts go into a bounded queue. A background task sends at most `monitor.telegram_max_per_min` per minute, about one per second, and merges alerts that pile up meanwhile.
- A merged message never cuts an alert: alerts that do not fit in one message are carried to the next one. Only a single alert longer than the Telegram limit is truncated.
- A 429 response is retried after its `retry_after`. 5xx and transport errors are retried with backoff.
- A 200 response counts as delivered only if its body is a valid Bot API answer with `"ok": true`. A cut or malformed body is retried like a transport error: delivery is unknown, and a duplicate is preferred over a lost alert.
- On queue overflow, alerts are dropped. Alerts that still fail after 3 attempts are given up. Both are counted at the top of the next message ("(N alerts dropped: queue full)", "(N earlier alerts could not be delivered)").
- The sent and failed counts are logged at shutdown.
- The token never appears in the logs: it is part of the request URL, so errors are logged without the URL, and every error string (including an API `description` that echoes the URL) has the token replaced by `<redacted>`. This applies to both alerts and commands.

### Telegram commands

The bot answers three **read-only** commands in its private chat:

| command | reply |
|---|---|
| `/pnl` | phase, position (side, size, average entry price, time held), entry premium and current premium (signal and raw, in bps), a warning if the executor's account position (exchange in live modes, simulated account in paper) differs from the strategy's, realized PnL (total and today UTC with its trade count), unrealized PnL of the open position (net of its fees and funding so far), equity (realized + unrealized), fees paid, number of trades, and the kill switch (rule, time, message) if one is active |
| `/trades` | the last 20 closed trades, newest first: exit time UTC, side, size, entry -> exit premium (bps), net PnL (USD), exit reason |
| `/help` | the list of commands |

- **Parsing.** Names are case-insensitive. The `/cmd@botname` form works, and trailing arguments are ignored. Any other command or text from the authorized chat gets the `/help` list.
- **Format and size.** Replies are plain text, with no `parse_mode`, so nothing needs escaping. They stay under Telegram's 4096-character limit: if `/trades` ever exceeded it, the oldest lines would be dropped with a note.
- **Data source.** The data comes from the same in-memory state the heartbeat reports.
  - The main loop publishes a snapshot every second, and right after entries, closed trades and kills.
  - Trade history is the ledger persisted in `state_<mode>.json`, so it survives restarts. It is the same history as `trades.csv`.
- **Configuration.** `monitor.telegram_commands_enabled` is `true` by default. Commands are active only when Telegram is enabled and `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` are set. The startup log says either `telegram commands enabled` or `telegram commands disabled: <reason>`.

**Authorization model.** An update is answered only when **all** of these hold:
- it is a new `message`;
- `message.chat.type == "private"`;
- `message.chat.id == TELEGRAM_CHAT_ID`;
- `message.from.id == TELEGRAM_CHAT_ID`;
- the sender is not a bot.

So `TELEGRAM_CHAT_ID` must be your Telegram **user id**: the chat id of your private chat with the bot, a positive number.
- With a group or channel id (negative), alerts still work but commands are disabled. Use a private chat if you want both.
- Everything else is ignored without a reply and logged at debug level at most: other users or chats, groups, supergroups, channels, edited messages, callback queries, messages without a sender, and non-text messages.
- Messages sent while the bot was down are skipped, so a restart does not answer a backlog. At startup a `getUpdates` call with `offset=-1` forgets the pending updates. Messages dated more than 5 seconds before the start are also ignored, a fallback in case that call fails; the 5 s tolerance absorbs clock skew between the host and Telegram, so a message sent in those last seconds before the start can still be answered.

**Read-only guarantee.**
- The command task only holds a copy of the state snapshot and an HTTP client. It has no handle on the engine, the executor or the kill switch.
- No command can change state, place or cancel orders, reset a kill, or stop the bot.

**Isolation from trading.**
- Long polling (`getUpdates`, 25 s timeout) runs in its own task. The main loop never waits on it: publishing the snapshot is a non-blocking `watch` channel update, and no lock is held during network I/O.
- Telegram failures are logged with the token redacted, then retried with exponential backoff from 1 s up to 60 s (a `retry_after` is honoured, also capped at 60 s). This covers network errors, HTTP 4xx / 5xx, a `409 Conflict` (another `getUpdates` consumer or a webhook on the same bot) and malformed answers.
- Repeated poll failures log at most one WARN per minute; the others are at debug level.
- A panic in the task is caught, logged, and the task restarts after 30 s. The update that caused it is not processed again.
- The task stops cleanly at shutdown.
- Replies are spaced at least 1.1 s apart. A reply rate-limited by Telegram (429) is retried once after its `retry_after`.
- Only one process may poll a given bot token. If two instances share a bot, both get `409 Conflict` errors: disable commands on all but one (`telegram_commands_enabled = false`). Alerts are unaffected.

## Kill rules

A kill stops new entries, cancels open orders and, with `risk.flatten_on_kill = true`, closes the position: maker first for `kill_flatten_maker_secs`, then IOC. It also alerts. The kill is persisted in `risk_state.json` and **survives restarts**.

| rule | default trigger |
|---|---|
| `hit_rate` | hit rate < 80% over the last 20 closed trades (hit = \|premium\| moved toward 0). It fires early, before 20 trades, once the misses make that outcome certain: the 5th miss |
| `persistent_premium` | \|signal\| > 150 bps continuously for 2 h (fresh data only), i.e. a regime change |
| `spec_change` | any change versus the baseline in: funding multiplier, oracle updater, deployer, delisted/halted flag, growth mode, deployer fee scale, last fee-scale change time, szDecimals, max leverage, margin mode, only-isolated, asset id. The coin disappearing from meta also triggers it |
| `daily_loss` | UTC-day PnL (realized + unrealized) ≤ −150 USD |
| `max_drawdown` | equity ≥ 300 USD below its peak |

To resume after investigating:

```sh
antbot kill-status                 # reason, later triggers, hit window, equity
# stop the bot first (reset-kill refuses while it runs)
antbot reset-kill --confirm        # clears the kill, hit window, drawdown peak, day start, spec baseline
```

After a spec change (for example growth mode turned off), update `[market] expected_*` and `[fees]` **before** resetting. The fee assumption behind the strategy may no longer hold.

## Risks and limitations

- **Not financial advice.** No warranty. Run paper first, then testnet, then mainnet at a small size. Mainnet trades real funds as soon as every guard passes.
- **Tiny sample.** About 17 days of history and 24 backtest trades. The edge may not exist, or may disappear once others trade it.
- **Pre-IPO oracle risk.** The oracle can jump on news (IPO, funding round, methodology change), the market can be halted or settled, and the "fair value" itself is a model. A position can gap far beyond the backtest's worst excursion (5.3%).
- **Fee regime.** The edge assumes growth-mode fees. Standard fees cost 10x more (6–18 bps per round trip). The deployer can switch growth mode off.
- **Liquidity.** The book is thin (about 30k USD within 20 levels per side). Above a few thousand USD per order, maker fills are slower and taker exits cost several bps. The 10k USD hard cap exists for this reason.
- **Fill model.** Paper and replay maker fills are optimistic: there is no adverse selection model, and the queue position is estimated. Live results will be worse.
- **Directional exposure.** The bot holds an unhedged position in a volatile pre-IPO perp for up to 8 h. Part of the backtest PnL came from favourable oracle moves.
- **Operational risk.** Venue outages, websocket gaps, clock drift, API changes or bugs in this software. Reconciliation, kill rules and alerts reduce these risks but do not remove them. Watch the alerts.
- **Key security.** Use an API wallet (no withdrawal rights; mainnet refuses a main wallet key), keep the env file at mode 600 in a mode-700 directory, never commit it, and never pass the key on the command line. Anyone with root on the VPS can still read it.
- **Hyperliquid limits.** The per-address action budget and the REST weight limits are respected client-side but are not guaranteed.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

CI (`.github/workflows/ci.yml`) runs the same steps on pushes to `main` and on pull requests, plus a Docker image build. Third-party actions are pinned by commit SHA.

License: proprietary / unlicensed (private repository).
