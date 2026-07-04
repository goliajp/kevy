#!/usr/bin/env bash
# v3.3 baseline arena — BARE FACE: kevy vs valkey 9.1, the real gap
# table (perfgate ratchets only prove "no regression vs ourselves";
# this measures the competitor). Discipline per the perf-arc charter:
#   - isolation (one server at a time, same cores), host loopback,
#     pinned client — the loopback_c50.sh fair-fight protocol;
#   - median-of-5 runs + sample stdev PER CELL; a gap smaller than
#     the stdev is reported as NOISE, not a gap;
#   - competitor version recorded in the output header.
#
# Output: markdown table rows for bench/PERF-LEDGER.md.
# Usage (lx64): bash bench/arena.sh <kevy-binary>
set -u
KBIN=${1:?usage: arena.sh <kevy-binary>}
KBIN=$(cd "$(dirname "$KBIN")" && pwd)/$(basename "$KBIN")
cd "$(dirname "$0")"

SRV_CORES=${SRV_CORES:-0-7}
CLI_CORES=${CLI_CORES:-8-15}
CLI_THREADS=${CLI_THREADS:-6}
N=${N:-2000000}
CONC=${CONC:-50}
PIPE=${PIPE:-16}
RUNS=${RUNS:-5}
PORT=7201
TESTS="get,set,incr,lpush,sadd,hset,zadd"

VALKEY_VER=$(docker run --rm valkey/valkey:9.1 valkey-server --version | grep -oE "v=[0-9.]+" | head -1)
echo "# arena bare face — $(date -u +%F) — kevy $($KBIN --version | head -1) vs valkey $VALKEY_VER"
echo "# protocol: -c $CONC -P $PIPE -n $N, server cores $SRV_CORES, client cores $CLI_CORES, median-of-$RUNS"

wait_ready() {
    for _ in $(seq 1 100); do
        redis-benchmark -h 127.0.0.1 -p "$PORT" -t ping -n 1 -q >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "!! port $PORT never came up" >&2
    return 1
}

# one full benchmark pass → "test rps" lines on stdout
bench_once() {
    taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p "$PORT" \
        -t "$TESTS" -n "$N" -c "$CONC" -P "$PIPE" --threads "$CLI_THREADS" -q 2>/dev/null \
        | grep -E "requests per second" \
        | sed -E 's/^([A-Z_]+):.* ([0-9.]+) requests per second.*/\1 \2/'
}

# run RUNS passes against the live server; emit "test median stdev"
measure() {
    local tmp
    tmp=$(mktemp)
    for _ in $(seq 1 "$RUNS"); do
        bench_once >> "$tmp"
    done
    python3 - "$tmp" <<'PY'
import sys, statistics
from collections import defaultdict
vals = defaultdict(list)
for line in open(sys.argv[1]):
    parts = line.split()
    if len(parts) == 2:
        vals[parts[0]].append(float(parts[1]))
for t in sorted(vals):
    v = sorted(vals[t])
    med = v[len(v)//2]
    sd = statistics.stdev(v) if len(v) > 1 else 0.0
    print(f"{t} {med:.0f} {sd:.0f}")
PY
    rm -f "$tmp"
}

run_server_and_measure() { # label, start-command...
    local label=$1
    shift
    "$@" >/dev/null 2>&1 &
    local SPID=$!
    sleep 1
    wait_ready || { kill $SPID 2>/dev/null; return 1; }
    measure | sed "s/^/$label /"
    kill $SPID 2>/dev/null
    wait $SPID 2>/dev/null
    docker rm -f arena-valkey >/dev/null 2>&1 || true
    sleep 1
}

echo "server test median stdev"
run_server_and_measure kevy \
    env KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads 8 --port $PORT --no-aof

run_server_and_measure valkey \
    docker run --rm --name arena-valkey --network host --cpuset-cpus "$SRV_CORES" \
    valkey/valkey:9.1 valkey-server --port $PORT --save '' --appendonly no --io-threads 8

echo "# gap rule: |kevy-valkey| <= max(stdev_kevy, stdev_valkey) => NOISE"
