#!/usr/bin/env bash
# clientbound — is the arena number the SERVER's ceiling or the LOAD
# GENERATOR's? (ROADMAP v3.4: "arena kevy 数字疑似 client 打满".)
#
# A throughput figure is only a ceiling if the thing being measured is
# what ran out first. arena pins the client to 8 cores and 6 threads; if
# those are saturated, every kevy cell is a FLOOR wearing a ceiling's
# clothes — and so is every ratio computed from it.
#
# The method is arena's, verbatim: drive redis-benchmark, let it ramp,
# then count the SERVER's own `total_commands_processed` across a window
# timed here. The one addition is that the client's CPU is sampled over
# the same window, so saturation is observed rather than inferred.
#
# Sweeps client width with the server fixed. If throughput climbs with
# the client, the client was the limit; if it plateaus while client CPU
# stays below saturation, the server is.
#
# Usage (lx64): bash bench/clientbound.sh <kevy-binary> [test]
set -u
KBIN=${1:?usage: clientbound.sh <kevy-binary> [test]}
KBIN=$(cd "$(dirname "$KBIN")" && pwd)/$(basename "$KBIN")
TEST=${2:-get}
cd "$(dirname "$0")"

SRV_CORES=${SRV_CORES:-0-7}
PORT=${PORT:-7791}
N=${N:-60000000}
RAMP=${RAMP:-1.0}
WINDOW=${WINDOW:-3.0}
CONC=${CONC:-50}
PIPE=${PIPE:-16}
RUNS=${RUNS:-3}

# (client cores, threads) — the arena point is 8c/6t, the first row.
LADDER=${LADDER:-"8-15:6 8-15:8 8-15:16 4-15:12 4-15:24"}

srv_cmds() {
    redis-cli -h 127.0.0.1 -p "$PORT" INFO stats 2>/dev/null | tr -d '\r' \
        | awk -F: '/^total_commands_processed:/ {print $2}'
}

# One cell, plus the client's CPU over the same window. Prints "ops cpu%".
cell() { # $1 = cli cores, $2 = threads
    local bpid c0 t0 c1 t1 cpu0 cpu1 ops cpu
    taskset -c "$1" redis-benchmark -h 127.0.0.1 -p "$PORT" \
        -t "$TEST" -n "$N" -c "$CONC" -P "$PIPE" --threads "$2" -q \
        >/dev/null 2>&1 &
    bpid=$!
    sleep "$RAMP"
    # utime+stime in clock ticks, straight from the generator's own proc
    # entry: what the client actually burned, not what top sampled.
    cpu0=$(awk '{print $14 + $15}' "/proc/$bpid/stat" 2>/dev/null || echo 0)
    c0=$(srv_cmds); t0=$(date +%s%N)
    sleep "$WINDOW"
    c1=$(srv_cmds); t1=$(date +%s%N)
    cpu1=$(awk '{print $14 + $15}' "/proc/$bpid/stat" 2>/dev/null || echo 0)
    kill "$bpid" 2>/dev/null; wait "$bpid" 2>/dev/null
    [ -z "$c0" ] || [ -z "$c1" ] && { echo "0 0"; return; }
    ops=$(awk -v c0="$c0" -v c1="$c1" -v t0="$t0" -v t1="$t1" \
        'BEGIN {printf "%.0f", (c1 - c0) / ((t1 - t0) / 1e9)}')
    # ticks / (seconds * HZ) * 100 = percent of ONE core; 800 = 8 cores.
    cpu=$(awk -v a="$cpu0" -v b="$cpu1" -v t0="$t0" -v t1="$t1" -v hz="$(getconf CLK_TCK)" \
        'BEGIN {printf "%.0f", (b - a) / ((t1 - t0) / 1e9) / hz * 100}')
    echo "$ops $cpu"
}

env KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads 8 --port "$PORT" --no-aof \
    >/dev/null 2>&1 &
SPID=$!
for _ in $(seq 1 100); do
    redis-benchmark -h 127.0.0.1 -p "$PORT" -t ping -n 1 -q >/dev/null 2>&1 && break
    sleep 0.1
done

echo "# clientbound — $TEST — server cores $SRV_CORES, median-of-$RUNS"
echo "# cpu% is the load generator's own utime+stime over the window; 100 = one core"
echo "cli_cores threads median_ops cpu_pct"
for point in $LADDER; do
    cores=${point%%:*}; threads=${point##*:}
    tmp=$(mktemp)
    for _ in $(seq 1 "$RUNS"); do cell "$cores" "$threads" >> "$tmp"; done
    python3 - "$tmp" "$cores" "$threads" <<'PY'
import sys, statistics
rows = [l.split() for l in open(sys.argv[1]) if len(l.split()) == 2]
ops = sorted(float(r[0]) for r in rows); cpu = sorted(float(r[1]) for r in rows)
print(f"{sys.argv[2]} {sys.argv[3]} {statistics.median(ops):.0f} {statistics.median(cpu):.0f}")
PY
    rm -f "$tmp"
done
kill $SPID 2>/dev/null; wait $SPID 2>/dev/null
