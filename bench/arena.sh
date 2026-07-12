#!/usr/bin/env bash
# v3.3 baseline arena — BARE FACE: kevy vs valkey 9.1, the real gap
# table (perfgate ratchets only prove "no regression vs ourselves";
# this measures the competitor). Discipline per the perf-arc charter:
#   - isolation (one server at a time, same cores), host loopback,
#     pinned client — the loopback_c50.sh fair-fight protocol;
#   - median-of-5 runs + sample stdev PER CELL; a gap smaller than
#     the stdev is reported as NOISE, not a gap;
#   - throughput is read from the SERVER's command counter over a wall
#     window timed here, NOT from redis-benchmark's reported rate. Under
#     `--threads` the benchmark's only exit is its own 250ms
#     showThroughput timer (redis-benchmark.c:52, :1653; without --threads
#     it stops in clientDone at :425), so totlatency is rounded UP to a
#     multiple of 250ms and the reported rate is quantized to N/(k*250ms)
#     and understated. This file used to describe that very effect —
#     "2M-request cells finished in ~0.5s and QUANTIZED LOW (ledger v1
#     recorded 3.99M/s; the ceiling ladder measured 5.3M/s truth)" — and
#     blamed N: 2M/0.5s is exactly 4.0M, the bucket 0.377s rounds up into.
#     Raising N shrinks the bucket relative to the run but never removes
#     it; at 8M and ~1.25s it was still 20% wide. Counting server-side
#     removes it outright. Both engines expose the same counter, so the
#     comparison stays like-for-like. See
#     bench/PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md.
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
# N is no longer the unit of measurement — the window below is. It only has
# to keep the load generator busy for RAMP + WINDOW at the SLOWEST engine and
# cell (valkey ZADD, ~1.6M ops/s): 60M requests is ~37s of headroom over a 4s
# measurement. The generator is killed once the window closes.
N=${N:-60000000}
RAMP=${RAMP:-1.0}
WINDOW=${WINDOW:-3.0}
CONC=${CONC:-50}
PIPE=${PIPE:-16}
RUNS=${RUNS:-5}
PORT=7201
TESTS="get set incr lpush sadd hset zadd"

VALKEY_VER=$(docker run --rm valkey/valkey:9.1 valkey-server --version | grep -oE "v=[0-9.]+" | head -1)
echo "# arena bare face — $(date -u +%F) — kevy $($KBIN --version | head -1) vs valkey $VALKEY_VER"
echo "# protocol: -c $CONC -P $PIPE, server cores $SRV_CORES, client cores $CLI_CORES, median-of-$RUNS"
echo "# measured: server-side total_commands_processed over a ${WINDOW}s window after a ${RAMP}s ramp (NOT redis-benchmark's rate — see the header)"

wait_ready() {
    for _ in $(seq 1 100); do
        redis-benchmark -h 127.0.0.1 -p "$PORT" -t ping -n 1 -q >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "!! port $PORT never came up" >&2
    return 1
}

# The engine's own count of the work it did. kevy and valkey both publish
# `total_commands_processed` in INFO stats, so this is one metric with one
# meaning on both sides of the table.
srv_cmds() {
    redis-cli -h 127.0.0.1 -p "$PORT" INFO stats 2>/dev/null | tr -d '\r' \
        | awk -F: '/^total_commands_processed:/ {print $2}'
}

# One cell: drive the load, let it settle, then count what the server did
# across a window we time ourselves.
bench_cell() { # $1 = test name -> ops/s
    local bpid c0 t0 c1 t1
    taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p "$PORT" \
        -t "$1" -n "$N" -c "$CONC" -P "$PIPE" --threads "$CLI_THREADS" -q \
        >/dev/null 2>&1 &
    bpid=$!
    sleep "$RAMP"
    c0=$(srv_cmds); t0=$(date +%s%N)
    sleep "$WINDOW"
    c1=$(srv_cmds); t1=$(date +%s%N)
    kill "$bpid" 2>/dev/null
    wait "$bpid" 2>/dev/null
    if [ -z "$c0" ] || [ -z "$c1" ]; then
        echo "!! $1: server counter unreadable on port $PORT" >&2
        printf "0"
        return
    fi
    awk -v c0="$c0" -v c1="$c1" -v t0="$t0" -v t1="$t1" \
        'BEGIN {printf "%.0f", (c1 - c0) / ((t1 - t0) / 1e9)}'
}

# RUNS passes per cell against the live server; emit "TEST median stdev"
measure() {
    local t tmp
    tmp=$(mktemp)
    for t in $TESTS; do
        for _ in $(seq 1 "$RUNS"); do
            printf "%s %s\n" "$(echo "$t" | tr '[:lower:]' '[:upper:]')" "$(bench_cell "$t")" >> "$tmp"
        done
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
    med = v[len(v) // 2]
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
