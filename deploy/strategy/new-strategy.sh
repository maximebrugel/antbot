#!/usr/bin/env bash
# Scaffold (or repair) one antbot strategy stack:
#
#   new-strategy.sh <name> [--mode paper|testnet|mainnet]
#
# creates $ANTBOT_STRATEGIES_DIR/<name>/ (default /root/projects/strategies):
#
#   compose.yaml   root:root   0644  copy of deploy/strategy/compose.yaml
#   config.toml    root:10001  0640  from config.example.toml, mode = --mode
#   .env           root:root   0600  from .env.example (non-secret settings)
#   secrets/       10001:10001 0700  one file per secret, 10001:10001 0400
#   data/          10001:10001 0750  bot state, journals, logs (/data)
#
# Idempotent: existing files are never overwritten (config.toml, .env and
# secret contents are the operator's); missing pieces are created and the
# ownership / modes above are re-applied on every run. Never reads or prints
# a secret value. Must run as root (chown to the container uid 10001).
set -euo pipefail

APP_UID=10001
APP_GID=10001
TEMPLATE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STRATEGIES_DIR="${ANTBOT_STRATEGIES_DIR:-/root/projects/strategies}"
# Secrets of the default service (compose.yaml): every file must exist.
SECRETS=(telegram_bot_token telegram_chat_id hl_private_key hl_account_address hl_vault_address)

usage() {
    echo "usage: $0 <name> [--mode paper|testnet|mainnet]" >&2
    exit 2
}
die() {
    echo "error: $*" >&2
    exit 1
}

[ $# -ge 1 ] || usage
NAME="$1"
shift
MODE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --mode)
            [ $# -ge 2 ] || usage
            MODE="$2"
            shift 2
            ;;
        --mode=*)
            MODE="${1#--mode=}"
            shift
            ;;
        -h | --help) usage ;;
        *) usage ;;
    esac
done

# Compose project names: lowercase letters, digits, '-' and '_'.
[[ "$NAME" =~ ^[a-z0-9][a-z0-9_-]{0,47}$ ]] ||
    die "invalid name '$NAME' (lowercase letters, digits, '-', '_'; max 48 chars)"
case "$MODE" in
    "" | paper | testnet | mainnet) ;;
    *) die "invalid --mode '$MODE' (paper | testnet | mainnet)" ;;
esac
[ "$(id -u)" -eq 0 ] || die "run as root (files are chowned to uid $APP_UID)"
for f in compose.yaml config.example.toml .env.example; do
    [ -f "$TEMPLATE_DIR/$f" ] || die "template $TEMPLATE_DIR/$f not found"
done

DIR="$STRATEGIES_DIR/$NAME"
umask 077
install -d -m 0755 -o root -g root "$STRATEGIES_DIR"
install -d -m 0750 -o root -g root "$DIR"
cd "$DIR"

created=()
kept=()

# compose.yaml: the template, unless the operator already has one.
if [ ! -e compose.yaml ]; then
    install -m 0644 -o root -g root "$TEMPLATE_DIR/compose.yaml" compose.yaml
    created+=(compose.yaml)
else
    kept+=(compose.yaml)
    if ! cmp -s compose.yaml "$TEMPLATE_DIR/compose.yaml"; then
        echo "note: compose.yaml differs from the template (kept):" \
            "diff $DIR/compose.yaml $TEMPLATE_DIR/compose.yaml"
    fi
fi

# config.toml: mode from --mode (paper by default) on creation only.
if [ ! -e config.toml ]; then
    tmp="$(mktemp config.toml.XXXXXX)"
    sed "s/^mode = \"paper\"\$/mode = \"${MODE:-paper}\"/" \
        "$TEMPLATE_DIR/config.example.toml" >"$tmp"
    grep -q "^mode = \"${MODE:-paper}\"\$" "$tmp" || die "could not set mode in config.toml"
    mv "$tmp" config.toml
    created+=(config.toml)
else
    kept+=(config.toml)
    current="$(sed -n 's/^mode = "\([a-z]*\)".*/\1/p' config.toml | head -n1)"
    if [ -n "$MODE" ] && [ "$current" != "$MODE" ]; then
        echo "warning: config.toml has mode = \"$current\", --mode $MODE ignored" \
            "(edit it by hand, with a fresh data dir)" >&2
    fi
fi
chown root:"$APP_GID" config.toml
chmod 0640 config.toml
if [ -e config.testnet.toml ]; then
    chown root:"$APP_GID" config.testnet.toml
    chmod 0640 config.testnet.toml
fi

# .env: non-secret settings; testnet / mainnet enable the HL_* file paths.
if [ ! -e .env ]; then
    tmp="$(mktemp .env.XXXXXX)"
    sed "s/__NAME__/$NAME/g" "$TEMPLATE_DIR/.env.example" >"$tmp"
    case "${MODE:-paper}" in
        testnet | mainnet)
            sed -i -e 's|^#HL_PRIVATE_KEY_FILE=|HL_PRIVATE_KEY_FILE=|' \
                -e 's|^#HL_ACCOUNT_ADDRESS_FILE=|HL_ACCOUNT_ADDRESS_FILE=|' "$tmp"
            ;;
    esac
    mv "$tmp" .env
    created+=(.env)
else
    kept+=(.env)
fi
chown root:root .env
chmod 0600 .env

# secrets/: the directory and every file the default service mounts.
install -d -m 0700 -o "$APP_UID" -g "$APP_GID" secrets
for s in "${SECRETS[@]}"; do
    if [ ! -e "secrets/$s" ]; then
        install -m 0400 -o "$APP_UID" -g "$APP_GID" /dev/null "secrets/$s"
        created+=("secrets/$s")
    fi
done
for f in secrets/*; do
    [ -f "$f" ] || continue
    chown "$APP_UID:$APP_GID" "$f"
    chmod 0400 "$f"
done

# data/: the container's /data.
if [ ! -d data ]; then
    created+=(data/)
fi
install -d -m 0750 -o "$APP_UID" -g "$APP_GID" data
# Never recurse into a running bot's data here: only the top level.

echo "strategy '$NAME' in $DIR"
[ ${#created[@]} -eq 0 ] || echo "created: ${created[*]}"
[ ${#kept[@]} -eq 0 ] || echo "kept:    ${kept[*]}"
echo
stat -c '%A %u:%g %n' . compose.yaml config.toml .env secrets secrets/* data

# Which secret files the .env actually uses, and which of those are empty.
missing=()
while IFS='=' read -r var path; do
    [ -n "$path" ] || continue
    host="secrets/${path#/run/secrets/}"
    if [ ! -s "$host" ]; then
        missing+=("$host ($var)")
    fi
done < <(grep -E '^[A-Z_]+_FILE=/run/secrets/' .env || true)

echo
if [ ${#missing[@]} -gt 0 ]; then
    echo "fill before starting (empty or missing, used by .env):"
    printf '  %s\n' "${missing[@]}"
    echo "  e.g.: read -rs V && printf '%s\\n' \"\$V\" > $DIR/secrets/<file> && unset V"
    echo
fi
echo "next:"
echo "  cd $DIR"
echo "  docker compose -p antbot-$NAME config --quiet && docker compose -p antbot-$NAME up -d"
echo "  docker compose -p antbot-$NAME ps"
