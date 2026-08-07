#!/usr/bin/env bash
# tailgate — the V3 tail-latency bars, measured by the in-process
# prober (bench/../crates/kevy/examples/tail_probe.rs).
#
# Two cells, two bars each (charter §2.1):
#   PING p99.9 <= 100ms  AND  reactor_tick_gap_max_us <= 100ms
#
#   A. mixed small-op storm — redis-benchmark SET/GET/INCR/LPUSH/SADD
#      at pipeline depth, 60s. The everyday shape.
#   B. AOF firehose — 64KiB SET at full rate, 60s (~1GB/s ingest).
#      The balance round measured 100-250ms stalls here and attributed
#      them to the AOF append path; this cell is the V3 train's target
#      and stays RED until that lands. A red cell is a true statement.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT=6320
WORK=$(mktemp -d "${TMPDIR:-/tmp}/tailgate-XXXXXX")
SRV=""
BENCH=""
fail=0
cleanup() {
    [ -n "$BENCH" ] && kill -9 "$BENCH" 2>/dev/null
    if [ -n "$SRV" ]; then
        kill -9 "$SRV" 2>/dev/null
        wait "$SRV" 2>/dev/null
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

cell() { # $1 = name, $2... = the redis-benchmark load args
    local name=$1; shift
    rm -rf "$WORK/$name"
    target/release/kevy --port $PORT --threads 4 --dir "$WORK/$name" \
        &>"$WORK/$name.log" &
    SRV=$!
    for _ in $(seq 50); do
        redis-cli -p $PORT ping 2>/dev/null | grep -q PONG && break
        sleep 0.2
    done
    redis-benchmark -p $PORT "$@" -q >/dev/null 2>&1 &
    BENCH=$!
    sleep 2 # let the load reach steady state before probing
    local out
    out=$(target/release/examples/tail_probe $PORT 60)
    kill -9 "$BENCH" 2>/dev/null; wait "$BENCH" 2>/dev/null; BENCH=""
    kill -9 "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; SRV=""
    echo "$name: $out"
    local p999 gap
    p999=$(echo "$out" | grep -oE 'p999us=[0-9]+' | cut -d= -f2)
    gap=$(echo "$out" | grep -oE 'reactor_gap_us=[0-9]+' | cut -d= -f2)
    if [ "${p999:-999999999}" -gt 100000 ]; then
        echo "  ✗ $name: PING p99.9 ${p999}us over the 100ms bar"
        fail=1
    fi
    if [ "${gap:-999999999}" -gt 100000 ]; then
        echo "  ✗ $name: reactor tick gap ${gap}us over the 100ms bar"
        fail=1
    fi
}

# A: the everyday mixed shape (1 KiB values, pipelined).
cell mixed -t set,get,incr,lpush,sadd -n 20000000 -r 1000000 -d 1024 -P 16 -c 50 --threads 4
# B: the firehose (64 KiB values — the balance round's stall shape).
cell firehose -t set -n 2000000 -r 100000 -d 65536 -P 8 -c 50 --threads 4

if [ "$fail" = 0 ]; then
    echo "tailgate: PASS (both cells inside the 100ms bars)"
else
    echo "tailgate: FAIL — a bar above is broken (cell B is the V3 train's named target)"
    exit 1
fi
