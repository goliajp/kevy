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

# Median-of-N (default 3, TAILGATE_RUNS=n overrides): the mixed cell's
# single-run reactor-gap spread measured 581-1657 ms across four runs
# (FINDING-2026-08-10-third-seat) — single runs cannot rank an A/B on
# these cells. Bars gate on the per-run MEDIAN; min..max is reported so
# a fix that only moves the tail is visible too.
RUNS=${TAILGATE_RUNS:-3}
median_of() { printf "%s\n" "$@" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

one_run() { # $1 = name, $2... = load args; echoes the probe line
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
    target/release/examples/tail_probe $PORT 60
    kill -9 "$BENCH" 2>/dev/null; wait "$BENCH" 2>/dev/null; BENCH=""
    kill -9 "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; SRV=""
}

cell() { # $1 = name, $2... = the redis-benchmark load args
    local name=$1; shift
    local p999s=() gaps=() out
    for i in $(seq "$RUNS"); do
        out=$(one_run "$name" "$@")
        echo "$name[$i/$RUNS]: $out"
        p999s+=("$(echo "$out" | grep -oE 'p999us=[0-9]+' | cut -d= -f2)")
        gaps+=("$(echo "$out" | grep -oE 'reactor_gap_us=[0-9]+' | cut -d= -f2)")
    done
    local p999 gap
    p999=$(median_of "${p999s[@]}")
    gap=$(median_of "${gaps[@]}")
    echo "$name: median p999us=$p999 reactor_gap_us=$gap" \
         "(p999 $(printf '%s\n' "${p999s[@]}" | sort -n | head -1)..$(printf '%s\n' "${p999s[@]}" | sort -n | tail -1)," \
         "gap $(printf '%s\n' "${gaps[@]}" | sort -n | head -1)..$(printf '%s\n' "${gaps[@]}" | sort -n | tail -1))"
    if [ "${p999:-999999999}" -gt 100000 ]; then
        echo "  ✗ $name: median PING p99.9 ${p999}us over the 100ms bar"
        fail=1
    fi
    if [ "${gap:-999999999}" -gt 100000 ]; then
        echo "  ✗ $name: median reactor tick gap ${gap}us over the 100ms bar"
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
