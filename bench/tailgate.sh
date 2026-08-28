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
# This gate measures what an AOF write costs the reactor, so it cannot
# run on a RAM-backed filesystem: writes are free there and every
# number is a fiction. Worse, on the box /tmp is a 32 GB tmpfs and the
# firehose FILLS it, killing the server mid-run — which the old
# `${x:-999999999}` defaults below rendered as four bars over the
# limit. A gate that cannot tell "no data" from "bad data" burns
# whoever reads it next. Refuse, with the fix in the message.
WORK_FSTYPE=$(df --output=fstype "$WORK" 2>/dev/null | tail -1 | tr -d ' ')
if [ "${WORK_FSTYPE:-}" = "tmpfs" ] && [ -z "${TAILGATE_ALLOW_TMPFS:-}" ]; then
    echo "tailgate: REFUSED — the work dir is on tmpfs ($WORK)."
    echo "  AOF writes to RAM cost nothing, so the bars would measure nothing,"
    echo "  and a full tmpfs kills the server mid-run. Point TMPDIR at a real"
    echo "  disk (on the box: TMPDIR=\$HOME/captmp), or set"
    echo "  TAILGATE_ALLOW_TMPFS=1 if you genuinely want the RAM numbers."
    rm -rf "$WORK"
    exit 2
fi
SRV=""
BENCH=""
fail=0
nomeasure=0
cleanup() {
    # The server below is waited for; the load generator was not, and it is
    # the same shape — a process still reaping when this gate returns is
    # residue billed to whatever runs next.
    if [ -n "$BENCH" ]; then
        kill -9 "$BENCH" 2>/dev/null
        wait "$BENCH" 2>/dev/null
    fi
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

one_run() { # $1 = name, $2 = reactor (auto|epoll), $3... = load args
    local name=$1 reactor=$2; shift 2
    rm -rf "$WORK/$name"
    local -a env_args=()
    [ "$reactor" = epoll ] && env_args=(KEVY_IO_URING=0)
    env "${env_args[@]}" target/release/kevy --port $PORT --threads 4 --dir "$WORK/$name" \
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

cell() { # $1 = name, $2 = reactor (auto|epoll), $3... = load args
    local name=$1 reactor=$2; shift 2
    local p999s=() gaps=() out p g
    for i in $(seq "$RUNS"); do
        out=$(one_run "$name" "$reactor" "$@")
        echo "$name[$i/$RUNS]: $out"
        p=$(echo "$out" | grep -oE 'p999us=[0-9]+' | cut -d= -f2)
        g=$(echo "$out" | grep -oE 'reactor_gap_us=[0-9]+' | cut -d= -f2)
        # An absent number is not a large one. This used to fall through
        # to the `${x:-999999999}` defaults and report the bars as
        # broken, which sent one debugging round chasing a regression
        # that was a dead server.
        if [ -z "$p" ] || [ -z "$g" ]; then
            echo "  ✗ $name[$i/$RUNS]: NO MEASUREMENT — the probe returned no numbers."
            echo "    The server usually died mid-run; its log tail:"
            tail -3 "$WORK/$name.log" 2>/dev/null | sed 's/^/      /'
            nomeasure=1
            return
        fi
        p999s+=("$p")
        gaps+=("$g")
    done
    local p999 gap
    p999=$(median_of "${p999s[@]}")
    gap=$(median_of "${gaps[@]}")
    echo "$name: median p999us=$p999 reactor_gap_us=$gap" \
         "(p999 $(printf '%s\n' "${p999s[@]}" | sort -n | head -1)..$(printf '%s\n' "${p999s[@]}" | sort -n | tail -1)," \
         "gap $(printf '%s\n' "${gaps[@]}" | sort -n | head -1)..$(printf '%s\n' "${gaps[@]}" | sort -n | tail -1))"
    if [ "$p999" -gt 100000 ]; then
        echo "  ✗ $name: median PING p99.9 ${p999}us over the 100ms bar"
        fail=1
    fi
    if [ "$gap" -gt 100000 ]; then
        echo "  ✗ $name: median reactor tick gap ${gap}us over the 100ms bar"
        fail=1
    fi
}

MIXED=(-t set,get,incr,lpush,sadd -n 20000000 -r 1000000 -d 1024 -P 16 -c 50 --threads 4)
FIREHOSE=(-t set -n 2000000 -r 100000 -d 65536 -P 8 -c 50 --threads 4)

# A: the everyday mixed shape (1 KiB values, pipelined).
cell mixed auto "${MIXED[@]}"
# B: the firehose (64 KiB values — the balance round's stall shape).
cell firehose auto "${FIREHOSE[@]}"

# C/D: the same two shapes on the poll reactor. It is the macOS main
# path, the pre-io_uring fallback, and the reactor every integration
# test forces — and it went a whole release line with no tail bar of
# its own, which is how it came to drain its own AOF writer lane ten
# times a second without anything turning red. A measured-green number
# with no bar on it is a number that regresses in silence.
#
# Skipped where io_uring was never in play (macOS is always this
# reactor, so cells A/B already measured it).
#
# Headroom, measured before this bar went in — three rounds of
# median-of-3 on the box, after the tick stopped draining the writer
# lane. Mixed medians 42-48 ms; firehose medians 37, 61 and 69 ms of
# the 100 ms bar, worst individual run 79.8 ms. The firehose cell's
# MEDIAN moves by nearly 2x between rounds, so a single reading near
# the bar means little and a trip means something: read a failure as a
# real signal before assuming flake.
if [ "$(uname -s)" = Linux ]; then
    cell mixed-epoll epoll "${MIXED[@]}"
    cell firehose-epoll epoll "${FIREHOSE[@]}"
fi

if [ "$nomeasure" != 0 ]; then
    echo "tailgate: NO MEASUREMENT — a cell produced no numbers, so no bar was"
    echo "  judged. This is an apparatus failure, not a regression; fix the run"
    echo "  and try again."
    exit 2
elif [ "$fail" = 0 ]; then
    echo "tailgate: PASS (every cell inside the 100ms bars)"
else
    echo "tailgate: FAIL — a bar above is broken (cell B is the V3 train's named target)"
    exit 1
fi
