#!/usr/bin/env bash
# Status of an antbot paper instance, read from its data dir (read-only).
#
# Usage: scripts/paper_status.sh [DIR]
#   DIR is the bot's data dir (the one holding antbot.lock, state_paper.json
#   and the CSV journals) or an instance dir containing a `data/` subdir.
#   Default: $ANTBOT_DATA_DIR, else ./data.
#
# Prints: process alive, last heartbeat, latest premium sample, current
# state / position, kill state, signal and trade counts, cumulative paper PnL.
# Needs: bash, jq, awk. Never talks to the network.
set -euo pipefail

dir="${1:-${ANTBOT_DATA_DIR:-data}}"
if [[ ! -f "$dir/state_paper.json" && ! -f "$dir/antbot.lock" && -d "$dir/data" ]]; then
  dir="$dir/data"
fi
if [[ ! -d "$dir" ]]; then
  echo "error: data dir '$dir' not found" >&2
  exit 2
fi
command -v jq >/dev/null || { echo "error: jq is required" >&2; exit 2; }

now_ms=$(( $(date +%s) * 1000 ))
fmt_ms() { date -u -d "@$(( $1 / 1000 ))" +%FT%TZ 2>/dev/null || echo "$1"; }
ago() { echo "$(( (now_ms - $1) / 1000 ))s ago"; }

echo "== antbot paper status: $dir ($(date -u +%FT%TZ))"

# --- process -----------------------------------------------------------------
lock="$dir/antbot.lock"
if [[ -f "$lock" ]]; then
  pid=$(jq -r '.pid // empty' "$lock")
  mode=$(jq -r '.mode // "?"' "$lock")
  started=$(jq -r '.started_ms // 0' "$lock")
  hb=$(jq -r '.heartbeat_ms // 0' "$lock")
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    alive="ALIVE"
  else
    alive="DEAD (stale lock)"
  fi
  echo "process:     $alive pid=$pid mode=$mode started=$(fmt_ms "$started") lock_heartbeat=$(ago "$hb")"
else
  echo "process:     NOT RUNNING (no antbot.lock; clean shutdown or never started)"
fi

# --- last heartbeat log line ---------------------------------------------------
logfile=$(ls -1 "$dir"/logs/antbot.log* 2>/dev/null | sort | tail -1 || true)
if [[ -n "$logfile" ]]; then
  hbline=$(grep -h 'heartbeat' "$logfile" | tail -1 || true)
  echo "heartbeat:   ${hbline:-<none in $logfile>}"
  errs=$(grep -cE ' (ERROR|WARN) ' "$logfile" || true)
  echo "log:         $logfile (WARN/ERROR lines today: ${errs:-0})"
else
  echo "heartbeat:   <no log file in $dir/logs>"
fi

# --- latest premium sample + range ---------------------------------------------
sig="$dir/signals.csv"
if [[ -f "$sig" ]]; then
  awk -F, '
    NR == 1 { for (i = 1; i <= NF; i++) c[$i] = i; next }
    {
      n++; last = $0
      p = $c["premium_bps"]; r = $c["raw_premium_bps"]
      if (p != "") { if (pmin == "" || p + 0 < pmin) pmin = p + 0; if (pmax == "" || p + 0 > pmax) pmax = p + 0 }
      if (r != "") { if (rmin == "" || r + 0 < rmin) rmin = r + 0; if (rmax == "" || r + 0 > rmax) rmax = r + 0 }
    }
    END {
      if (n == 0) { print "premium:     <no samples yet>"; exit }
      split(last, f, ",")
      printf "premium:     %s avg=%s raw=%s api=%s avg_ready=%s stale=%s oracle=%s mark=%s bid=%s ask=%s\n",
        f[c["ts_utc"]], f[c["premium_bps"]], f[c["raw_premium_bps"]], f[c["api_premium_bps"]],
        f[c["avg_ready"]], f[c["stale"]], f[c["oracle_px"]], f[c["mark_px"]], f[c["best_bid"]], f[c["best_ask"]]
      if (pmin == "") { pmin = "n/a"; pmax = "n/a" }
      if (rmin == "") { rmin = "n/a"; rmax = "n/a" }
      printf "range:       avg premium [%s, %s] bps, raw [%s, %s] bps over %d samples\n", pmin, pmax, rmin, rmax, n
    }' "$sig"
else
  echo "premium:     <no signals.csv>"
fi

# --- state / position ------------------------------------------------------------
st="$dir/state_paper.json"
if [[ -f "$st" ]]; then
  jq -r --argjson now "$now_ms" '
    def r2: . * 100 | round / 100;
    .strategy as $s | .executor.account as $a |
    "state:       phase=\($s.phase) saved=\(((($now - .saved_ms) / 1000) | floor))s ago entries_allowed=\($s.entries_allowed) cooldown_until=\(if ($s.cooldown_until_ms // 0) > $now then ($s.cooldown_until_ms / 1000 | todate) else "-" end)",
    (if $s.position then
      ($s.position | "position:    \(.side) size=\(.size) avg_entry=\((.entry_notional / .entry_sz) * 10000 | round / 10000) notional=\(.entry_notional | r2) USD entry_premium=\((.entry_premium_bps // 0) | r2) bps opened=\(.opened_ms / 1000 | floor | todate) held=\((($now - .opened_ms) / 60000) | floor) min exited_sz=\(.exit_sz)")
     else "position:    flat" end),
    (if $s.working then "working:     \($s.working | tostring)" else empty end),
    "account:     sim position_sz=\($a.position_sz) fills=\($a.fills) volume=\($a.volume_usd | r2) USD fees=\($a.fees_usd | r2) funding=\($a.funding_usd | r2)",
    "ledger:      realized_pnl=\(.ledger.realized_pnl_usd | r2) USD closed_trades=\(.ledger.trades | length)"
  ' "$st"
else
  echo "state:       <no state_paper.json>"
fi
rs="$dir/risk_state.json"
if [[ -f "$rs" ]]; then
  jq -r '"kill:        \(if .kill then (.kill | tostring) else "not killed" end) (trades_closed=\(.trades_closed), peak_equity=\(.peak_equity_usd * 100 | round / 100))"' "$rs"
fi

# --- counts and PnL from the journals ---------------------------------------------
ord="$dir/orders.csv"
if [[ -f "$ord" ]]; then
  awk -F, '
    NR == 1 { for (i = 1; i <= NF; i++) c[$i] = i; next }
    $c["event"] == "place" { places++; if ($c["purpose"] == "entry") entries++ }
    $c["event"] == "fill"  { fills++; fees += $c["fee_usd"]; if ($c["crossed"] == "true") taker++ }
    $c["status"] ~ /rejected/ { rej++ }
    END { printf "orders:      %d placed (%d entry), %d fills (%d taker), %d rejected, fill fees %.4f USD\n", places, entries, fills, taker, rej, fees }
  ' "$ord"
fi
tr="$dir/trades.csv"
if [[ -f "$tr" ]]; then
  awk -F, '
    NR == 1 { for (i = 1; i <= NF; i++) c[$i] = i; next }
    {
      n++; pnl += $c["pnl_usd"]; gross += $c["gross_pnl_usd"]; fees += $c["fees_usd"]; fund += $c["funding_usd"]
      bps += $c["pnl_bps"]; if ($c["pnl_usd"] + 0 > 0) win++
      last = sprintf("%s %s %s->%s pnl=%s USD (%s bps) reason=%s", $c["exit_ts_utc"], $c["side"], $c["entry_px"], $c["exit_px"], $c["pnl_usd"], $c["pnl_bps"], $c["reason"])
    }
    END {
      if (n == 0) { print "trades:      0 closed"; exit }
      printf "trades:      %d closed, %d with pnl>0 (%.1f%%), avg %.2f bps\n", n, win, 100 * win / n, bps / n
      printf "paper PnL:   net %.4f USD (gross %.4f, fees %.4f, funding %.4f)\n", pnl, gross, fees, fund
      printf "last trade:  %s\n", last
    }' "$tr"
fi
