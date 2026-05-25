#!/usr/bin/env bash
# A/B a kevy binary under a SERVER-SATURATING config: few shards + a strong
# client (many cores, deep pipeline) so the server, not the load generator, is
# the bottleneck — otherwise client-bound -c50 hides server-side CPU wins.
# Args: <binary-path> <label>. io_uring GET (the -c50-optimal read path).
set -u
KBIN=${1:?bin}
LABEL=${2:?label}
SRV_CORES=${SRV_CORES:-0-3}        # 4 shards
SHARDS=${SHARDS:-4}
CLI_CORES=${CLI_CORES:-4-15}       # 12 client cores
CLI_THREADS=${CLI_THREADS:-12}
PIPE=${PIPE:-64}
N=${N:-6000000}

overall() { printf '%s' "$1" | tr '\r' '\n' | grep "^$2: rps=" | tail -1 \
  | sed -E 's/^[A-Z]+: rps=[0-9.]+ \(overall: ([0-9.]+)\).*/\1/'; }
wait_ready() { for _ in $(seq 1 100); do
  redis-benchmark -h 127.0.0.1 -p "$1" -t ping -c 1 -n 1 -q >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }

run() { # op
  local raw
  raw=$(taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p 7001 -t "$1" \
    -n "$N" -c 50 -P "$PIPE" --threads "$CLI_THREADS" -q 2>/dev/null)
  printf '[%s] %s -> %s rps\n' "$LABEL" "$1" "$(overall "$raw" "${1^^}")"
}

echo "### $LABEL  io_uring ${SHARDS}sh on $SRV_CORES | client $CLI_CORES x$CLI_THREADS -P$PIPE n=$N"
KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$SHARDS" --port 7001 --no-aof >/tmp/sat.log 2>&1 &
P=$!
wait_ready 7001 || { echo "down"; kill $P; exit 1; }
redis-benchmark -h 127.0.0.1 -p 7001 -t set -n 300000 -P 16 -q >/dev/null 2>&1
run get; run get; run set; run set
kill "$P" 2>/dev/null; wait "$P" 2>/dev/null
echo "### SAT_DONE $LABEL"
