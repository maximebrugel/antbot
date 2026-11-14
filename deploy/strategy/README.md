# One Compose stack per strategy

Each strategy (one coin, one mode, one account) runs as its own Compose project `antbot-<name>`, from its own directory on the host. Stacks share the `antbot` image and nothing else: separate config, secrets, data dir, container and Telegram header.

Files here:

| file | role |
|---|---|
| `compose.yaml` | the stack: hardened `antbot` service plus the optional `paper` / `testnet` profiles |
| `config.example.toml` | strategy config: the keys a strategy usually changes; every other key comes from `config/default.toml` |
| `.env.example` | non-secret settings and the `*_FILE` secret paths |
| `new-strategy.sh` | scaffolds (or repairs) `/root/projects/strategies/<name>/` with the layout and permissions below |

## Quick start

```sh
# 1. Image (from the repo root). Tag with the git SHA to pin and roll back.
docker build --build-arg GIT_SHA=$(git rev-parse --short HEAD) \
  -t antbot:latest -t antbot:$(git rev-parse --short HEAD) .

# 2. Scaffold (as root). Default mode: paper.
deploy/strategy/new-strategy.sh ant-paper --mode paper

# 3. Edit config.toml (coin, sizing, rest_refill_per_sec) and fill the
#    secret files the script lists as empty, e.g. (no echo, no history):
cd /root/projects/strategies/ant-paper
read -rs V && printf '%s\n' "$V" > secrets/telegram_bot_token && unset V

# 4. Validate, start, follow.
docker compose -p antbot-ant-paper run --rm --no-deps antbot check-config
docker compose -p antbot-ant-paper up -d
docker compose -p antbot-ant-paper ps          # (healthy) after up to ~2 min
docker compose -p antbot-ant-paper logs -f
```

`.env` sets `COMPOSE_PROJECT_NAME=antbot-<name>`, so a plain `docker compose ...` run from the strategy directory targets the same project. The examples spell out `-p antbot-<name>` anyway, so a command typed in the wrong directory fails instead of acting on another stack.

The script is idempotent. Re-run it after adding or editing a secret file: it never overwrites `config.toml`, `.env`, `compose.yaml` or a secret, creates whatever is missing, re-applies the ownership and modes, and lists the secret files `.env` uses that are still empty. `ANTBOT_STRATEGIES_DIR` changes the parent directory.

## Host layout and permissions

```
/root/projects/strategies/<name>/        root:root    0750
  compose.yaml                           root:root    0644
  config.toml                            root:10001   0640   -> /app/config/config.toml (ro)
  .env                                   root:root    0600   non-secret settings only
  secrets/                               10001:10001  0700
    telegram_bot_token, telegram_chat_id,
    hl_private_key, hl_account_address,
    hl_vault_address                     10001:10001  0400   -> /run/secrets/<file>
  data/                                  10001:10001  0750   -> /data (rw)
```

- **The container runs as `10001:10001`** (the image's `antbot` user), with a read-only root filesystem, no capabilities, `no-new-privileges`, and `init: true` (tini as PID 1 forwards `SIGTERM`). It writes only to `/data`.
- **Secrets.** In standalone Compose (no Swarm), file-based `secrets:` are bind mounts. They keep the owner and mode they have on the host, so ownership must be set on the host: `10001:10001 0400` makes them readable by the bot only. The bot reads them through `TELEGRAM_BOT_TOKEN_FILE`, `HL_PRIVATE_KEY_FILE`, and so on (see `src/secrets.rs`). The container environment carries only the paths. Every secret listed by the `antbot` service must exist as a file. An empty file is fine as long as its `*_FILE` line in `.env` stays unset. A `*_FILE` pointing at an empty file is a startup error.
- **`.env` holds no secrets.** `compose.yaml` passes an explicit list of variables to the container (it uses no `env_file:`), so a raw `HL_PRIVATE_KEY=...` mistakenly added to `.env` never reaches the bot.
- **`config.toml`** is non-secret and read-only in the container. `data_dir = "/data"`, and the image also sets `ANTBOT_DATA_DIR=/data`. No host path appears anywhere in the config.
- **No published ports.** `/healthz` and `/metrics` listen on `127.0.0.1:9100` inside the container's own network namespace (`ANTBOT_HEALTH_LISTEN`). The Docker healthcheck is `antbot healthcheck`, which needs no curl and writes nothing. Probe the endpoints from the same namespace:

  ```sh
  docker exec antbot-<name> antbot healthcheck
  docker run --rm --network container:antbot-<name> busybox:1.37 \
    wget -qO- http://127.0.0.1:9100/metrics
  ```

## Services and profiles

| service | profile | command | data dir | secrets |
|---|---|---|---|---|
| `antbot` | (default) | `run` in the mode of `config.toml` | `/data` | `telegram_*`, `hl_*` (used if `.env` sets the `*_FILE`) |
| `antbot-paper` | `paper` | `run --mode paper` | `/data/paper` | none (no Telegram, see below) |
| `antbot-testnet` | `testnet` (off by default) | `run --mode testnet` with `./config.testnet.toml` | `/data/testnet` | `testnet_hl_private_key`, `testnet_hl_account_address` |

```sh
docker compose -p antbot-<name> --profile paper up -d antbot-paper
docker compose -p antbot-<name> --profile testnet up -d antbot-testnet
```

- Profile services get separate data dirs, because `risk_state.json` holds a spec baseline and hit-rate window that must never mix across modes. They get no Telegram either: two `getUpdates` pollers on one bot token conflict (HTTP 409).
- Before starting the testnet profile, create `config.testnet.toml` (a copy of `config.toml` with `mode = "testnet"` and the testnet coin's `[market]` spec) and both `testnet_*` secret files, then re-run `new-strategy.sh <name>` to fix their permissions. Compose refuses to start the service while a file is missing.
- **Mainnet is never started by accident.** `new-strategy.sh <name> --mode mainnet` writes `mode = "mainnet"`, which the bot refuses until ALL of these hold:
  - `.env` has `ANTBOT_MAINNET_FLAG=--mainnet`, which adds the `--mainnet` CLI flag;
  - `.env` has `ANTBOT_CONFIRM_MAINNET` set to the exact value from the repo README ("Running on mainnet");
  - `HL_PRIVATE_KEY_FILE` points at an **API wallet** key and `HL_ACCOUNT_ADDRESS_FILE` at the master account (a main wallet key is refused).

  The template never sets the first two lines. Until they are set, the container exits at startup with "mainnet mode refused", before any network call. `--mainnet` with a paper or testnet config is also an error. Check the setup with `docker compose -p antbot-<name> run --rm --no-deps antbot check-config --mainnet`.

## Several bots on one host

- **REST budget.** Hyperliquid allows 1,200 request weight per minute per public IP, i.e. 20 weight/s, and every container on the host shares it. Keep the sum of `[timing].rest_refill_per_sec` over all running bots at or below 20 (e.g. 4 bots at 5.0, 2 at 10.0). Count every running bot, including any non-Docker instance on the same IP. `antbot check-config` prints this instance's share. The template default is 5.0.
- **One API wallet per running bot.** Nonces are per signer, and two processes signing with the same API wallet can collide or invalidate each other's nonces. Create one API wallet per strategy (Hyperliquid UI: More > API). An API wallet can trade but cannot withdraw.
- **One sub-account / vault per strategy.** Positions, margin and open orders are per account, and the bot cancels unknown orders and adopts a position it finds. Two strategies on one account would fight over them. Give each strategy its own sub-account (`[live].vault_address` or `secrets/hl_vault_address` with `HL_VAULT_ADDRESS_FILE`). Its API wallet stays an agent of the master account.
- **Telegram.** One bot token can serve every stack: each alert carries `[<instance> <mode> <coin>]`, where the instance is `ANTBOT_INSTANCE_NAME`. Only one stack per token may have `telegram_commands_enabled = true`.
- **Memory.** Each container has `mem_reservation: 64m` / `mem_limit: 256m` and `pids_limit: 128`. A warm paper bot peaked at 15.3 MiB RSS, with 6 threads and 7 pids (measured 2026-11-14, see `compose.yaml`).

## Monitoring: Dozzle and Beszel

The monitoring stack (`/root/projects/monitoring`, project `monitoring`) reads the Docker API through a read-only socket proxy. Antbot stacks need no change, no port and no network for that:

- **Dozzle** lists every container, so `antbot-<name>` shows up on its own. It groups stacks under the `dev.dozzle.group=antbot` label and streams the json-file logs (10 MB x 5 per container, plus the bot's own daily files in `data/logs/`).
- **Beszel** (agent) reports per-container CPU, memory and network for `antbot-<name>`.
- `docker compose -p monitoring ps` shows that the monitoring stack is up. It is managed separately: never restart or edit it from here.

## Day-to-day

```sh
P="docker compose -p antbot-<name>"
$P ps                                          # state and health
$P logs -f --since 10m                         # "websocket connected", "heartbeat"
$P stop                                        # graceful (SIGTERM, 45 s grace)
$P up -d                                       # start / apply compose or .env changes
$P restart                                     # after editing config.toml
$P run --rm --no-deps antbot kill-status
$P stop antbot && $P run --rm --no-deps antbot reset-kill --confirm && $P up -d
# Upgrade: build a new image tag, set ANTBOT_TAG in .env, then
$P up -d                                       # recreates the container; state resumes from data/
```

`run --rm` one-shots use the same user, mounts and secrets as the service. `reset-kill` refuses to run while the bot holds `data/antbot.lock`.

## Migrating a live (non-Docker) bot into a stack

Do this only while the bot is **flat** (no position, no resting order). Use a calm window, not one with an entry signal forming.

1. **Scaffold**: `new-strategy.sh <name> --mode <mode>`, then copy the live config's values into `config.toml`, keeping `data_dir = "/data"`. Fill the secret files from the live env file (never paste them into `.env`). Set the same API wallet, account and vault the live bot uses. Re-run the script to fix permissions. Leave `ANTBOT_MAINNET_FLAG` / `ANTBOT_CONFIRM_MAINNET` unset for now.
2. **Check it is flat**: the last heartbeat shows `phase=flat`, `position=0` and no `working_order`. Confirm in the Hyperliquid UI that the account has no position and no open order.
3. **Stop the live bot with SIGTERM** (`kill -TERM <pid>`, or `systemctl stop`). Never use SIGKILL. Wait for the `shutting down` line and for the process to exit. The shutdown cancels the bot's open orders and saves its state.
4. **Cold backup** of the stopped bot's data dir, kept apart: `cp -a <live>/data <live>/data.backup-$(date -u +%Y%m%dT%H%M%SZ)`.
5. **Copy the data** into the stack, without the run lock:
   `rsync -a --exclude antbot.lock <live>/data/ /root/projects/strategies/<name>/data/`
   (or `cp -a` followed by `rm data/antbot.lock`).
6. **Ownership**: `chown -R 10001:10001 /root/projects/strategies/<name>/data`.
7. **Mainnet only**: now set `ANTBOT_MAINNET_FLAG` and `ANTBOT_CONFIRM_MAINNET` in `.env`, and check that `docker compose -p antbot-<name> run --rm --no-deps antbot check-config --mainnet` passes.
8. **Start**: `docker compose -p antbot-<name> up -d`.
9. **Verify**:
   - the logs show `antbot starting` with the expected mode, account and vault, then `asset resolved`, `websocket connected`, `main loop started` and the reconciliation result;
   - the state is resumed: same trade count and equity as before the stop;
   - the status reaches `(healthy)` and `antbot healthcheck` exits 0;
   - a Telegram startup alert arrives with the new instance header.

   Keep the old directory and the backup until the stack has run cleanly for a while. Never start the old bot again on the same account while the stack runs.

Rollback: `docker compose -p antbot-<name> stop` (wait for the exit), copy `data/` back (without `antbot.lock`), and start the old bot.
