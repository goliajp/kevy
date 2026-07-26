#!/usr/bin/env bash
# allocgate-mem — the memory half of M3, measured for both builds.
#
# The arc exists because resident memory ran 2.24x the logical bound on
# ~400 B values under demote/promote churn, and neither malloc_trim nor
# MALLOC_ARENA_MAX moved it
# (bench/PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md). This
# runs that shape against an allocator-off and an allocator-on binary and
# reports what each does to the ratio.
#
# It is deliberately narrow. The pub/sub A/B says the allocator costs
# throughput on that path
# (bench/PERF-FINDING-2026-07-26-header-free-costs-a-cache-line.md); this
# is the other side of the trade, and nobody can weigh the two without
# both numbers.
#
#   ALLOCGATE_BIN_OFF=... ALLOCGATE_BIN_ON=... bash bench/allocgate-mem.sh
#
# Knobs: KEYS, VAL, BUDGET_MB, DRAIN.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
PY="$HERE/capacity_envelope.py"
PORT=${MEMGATE_PORT:-7415}
KEYS=${KEYS:-2000000}
VAL=${VAL:-400}
BUDGET_MB=${BUDGET_MB:-512}
DRAIN=${DRAIN:-20}

[ "$(id -u)" -ne 0 ] || { echo "refusing to run as root — use an unprivileged bench account" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 required" >&2; exit 2; }

BIN_OFF=${ALLOCGATE_BIN_OFF:?set ALLOCGATE_BIN_OFF}
BIN_ON=${ALLOCGATE_BIN_ON:?set ALLOCGATE_BIN_ON}

rss_kb() {
  if [ -r "/proc/$1/status" ]; then awk '/^VmRSS:/{print $2}' "/proc/$1/status"
  else ps -o rss= -p "$1" 2>/dev/null | tr -d ' '; fi
}

# Cleanup is deliberately NOT an EXIT trap. A trap is inherited by
# background subshells, so killing a background sampler fires it and the
# directory the main script is still reading disappears underneath it —
# which is exactly what happened on the first run here: the RSS samples
# vanished and every ratio came back zero.
CLEAN=()
cleanup() { for d in "${CLEAN[@]:-}"; do [ -n "$d" ] && rm -rf "$d"; done; }

# One run: boot, ingest, drain, report peak used_memory and peak RSS.
run_one() { # $1 = binary, $2 = label
  local bin=$1 label=$2 dir srv used_peak=0 rss_peak=0 u r
  dir=$(mktemp -d "${TMPDIR:-/tmp}/agmem-XXXXXX")
  CLEAN+=("$dir")
  # No AOF: this measures memory, and an append log only adds buffers to
  # the RSS being compared. It also keeps shutdown quick — the first
  # version waited on a clean SIGTERM shutdown that spends minutes
  # flushing two million keys, which looked exactly like a hang.
  env KEVY_TIER_BUDGET="$((BUDGET_MB * 1024 * 1024))" KEVY_BIND=127.0.0.1 \
    "$bin" --port "$PORT" --dir "$dir" --threads 1 --no-aof >"$dir/server.log" 2>&1 &
  srv=$!
  for _ in $(seq 1 100); do
    python3 "$PY" info --port "$PORT" >/dev/null 2>&1 && break
    sleep 0.2
  done
  # Sample in the background, load in the foreground. The peak is what an
  # operator's cgroup sees and it is not the steady state, so it has to be
  # watched rather than read at the end.
  #
  # The first attempt had it the other way round and hung: a finished
  # background job stays a zombie until it is waited on, and `kill -0`
  # succeeds on a zombie, so the loop never saw the loader finish. There
  # is no EXIT trap in this script precisely so that killing the sampler
  # below is harmless — see the note above `cleanup`.
  ( while :; do rss_kb "$srv"; sleep 0.5; done ) >"$dir/rss" 2>/dev/null &
  local samp=$!
  python3 "$PY" load-b6 --port "$PORT" --keys "$KEYS" --val "$VAL" --seed 2 >"$dir/load.log" 2>&1 \
    || echo "$label: load reported an error (see $dir/load.log)" >&2
  sleep "$DRAIN"
  kill "$samp" 2>/dev/null || true
  u=$(python3 "$PY" info --port "$PORT" 2>/dev/null | sed -n 's/^used_memory:\([0-9]*\).*/\1/p' | head -1)
  used_peak=${u:-0}
  r=$(sort -n "$dir/rss" 2>/dev/null | tail -1)
  rss_peak=$(( ${r:-0} * 1024 ))
  local cold
  cold=$(python3 "$PY" info --port "$PORT" 2>/dev/null | sed -n 's/^cold_keys:\([0-9]*\).*/\1/p' | head -1)
  # SIGKILL, not SIGTERM: the numbers are already taken, and a clean
  # shutdown here only buys a wait.
  kill -9 "$srv" 2>/dev/null || true
  wait "$srv" 2>/dev/null || true
  awk -v l="$label" -v u="$used_peak" -v r="$rss_peak" -v c="${cold:-0}" 'BEGIN {
    printf "%-4s used_memory %8.1f MB   RSS %8.1f MB   frag %5.2fx   cold_keys %s\n",
           l, u/1048576, r/1048576, (u > 0 ? r/u : 0), c }'
  printf '%s %s\n' "$used_peak" "$rss_peak" >>"${RESULTS:-/dev/null}"
}

echo "allocgate-mem — $KEYS x ${VAL}B on a ${BUDGET_MB}MB budget, one shard"
echo "the shape that produced 2.24x: sub-mmap-threshold values, demoted under churn"
echo

# Order matters as little as possible: run each twice, alternating, so a
# box that drifts across the run cannot decide the verdict on its own.
for round in 1 2; do
  if [ "$round" = 1 ]; then
    run_one "$BIN_OFF" "OFF"; run_one "$BIN_ON" "ON"
  else
    run_one "$BIN_ON" "ON"; run_one "$BIN_OFF" "OFF"
  fi
done

cleanup
