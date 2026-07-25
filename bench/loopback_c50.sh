#!/usr/bin/env bash
# Clean 3-way -c50 -P16 throughput on a single 16-core box. Two artifacts to
# avoid: (1) busy-poll-starves-client -> client pinned to disjoint cores; (2)
# kevy's busy-poll starves any *co-located* competitor -> run each server in
# ISOLATION (start, bench, stop) so every server gets the same idle cores 0-9.
# All in-memory, all host-loopback (no docker bridge / NAT). Same core budget
# for every server => fair fight.
set -u
cd "$(dirname "$0")"

SRV_CORES=${SRV_CORES:-0-9}
CLI_CORES=${CLI_CORES:-10-15}
CLI_THREADS=${CLI_THREADS:-6}
# N is no longer the unit of measurement — the window is. It only has to keep
# the generator busy through RAMP + WINDOW at the slowest engine.
N=${N:-60000000}
RAMP=${RAMP:-1.0}
WINDOW=${WINDOW:-3.0}
PIPE=${PIPE:-16}
CONC=${CONC:-50}
KEVY_THREADS=${KEVY_THREADS:-10}
KBIN=${KBIN:-/root/kevy/target/release/kevy}

wait_ready() { # port
  for _ in $(seq 1 100); do
    redis-benchmark -h 127.0.0.1 -p "$1" -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "!! port $1 never came up"; return 1
}

# Throughput from the server's own counter over a window timed here, NOT from
# redis-benchmark's final "requests per second" line. Under --threads that
# line is quantized: the benchmark's only exit is its 250ms showThroughput
# timer (redis-benchmark.c:52, :1653), so elapsed is rounded up to a multiple
# of 250ms and the rate is snapped to N/(k*250ms) and understated.
srv_cmds() { # port
  redis-cli -h 127.0.0.1 -p "$1" INFO stats 2>/dev/null | tr -d '\r' \
    | awk -F: '/^total_commands_processed:/ {print $2}'
}

bench() { # port label
  local t bpid c0 t0 c1 t1
  for t in get set; do
    taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p "$1" \
      -t "$t" -n "$N" -c "$CONC" -P "$PIPE" --threads "$CLI_THREADS" -q \
      >/dev/null 2>&1 &
    bpid=$!
    sleep "${RAMP:-1.0}"
    c0=$(srv_cmds "$1"); t0=$(date +%s%N)
    sleep "${WINDOW:-3.0}"
    c1=$(srv_cmds "$1"); t1=$(date +%s%N)
    kill "$bpid" 2>/dev/null
    wait "$bpid" 2>/dev/null
    awk -v l="$2" -v t="$(echo "$t" | tr '[:lower:]' '[:upper:]')" \
        -v c0="$c0" -v c1="$c1" -v t0="$t0" -v t1="$t1" \
      'BEGIN {printf "[%s] %s: %.0f requests per second (server counter)\n", l, t, (c1-c0)/((t1-t0)/1e9)}'
  done
}

run_two() { # port label
  wait_ready "$1" || return
  redis-benchmark -h 127.0.0.1 -p "$1" -t set -n 200000 -P 16 -q >/dev/null 2>&1 # warm
  bench "$1" "$2"
  bench "$1" "$2"
}

echo "### load: $(uptime)"
echo "### ISOLATED runs. server cores=$SRV_CORES  client cores=$CLI_CORES x$CLI_THREADS  n=$N -c$CONC -P$PIPE"

# ---- kevy (epoll), host process, $KEVY_THREADS shards ----
echo "=== kevy-${KEVY_THREADS}sh (epoll) ==="
KEVY_IO_URING=0 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$KEVY_THREADS" \
  --port 7001 --no-aof >/tmp/kevy_srv.log 2>&1 &
KPID=$!
run_two 7001 "kevy-${KEVY_THREADS}sh"
kill "$KPID" 2>/dev/null; wait "$KPID" 2>/dev/null

# ---- kevy (io_uring) ----
echo "=== kevy-${KEVY_THREADS}sh (io_uring) ==="
KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" \
  --threads "$KEVY_THREADS" --port 7001 --no-aof >/tmp/kevy_uring.log 2>&1 &
KPID=$!
run_two 7001 "kevy-uring"
kill "$KPID" 2>/dev/null; wait "$KPID" 2>/dev/null

# ---- valkey 9.1 default (single exec thread) ----
echo "=== valkey 9.1 (default) ==="
docker run -d --rm --name bench_v --network host --cpuset-cpus "$SRV_CORES" \
  valkey/valkey:9.1 valkey-server --port 7002 --save '' --appendonly no >/dev/null 2>&1
run_two 7002 "valkey-def"
docker rm -f bench_v >/dev/null 2>&1

# ---- valkey 9.1 io-threads ----
echo "=== valkey 9.1 (io-threads=$KEVY_THREADS) ==="
docker run -d --rm --name bench_v --network host --cpuset-cpus "$SRV_CORES" \
  valkey/valkey:9.1 valkey-server --port 7002 --save '' --appendonly no \
  --io-threads "$KEVY_THREADS" >/dev/null 2>&1
run_two 7002 "valkey-iot"
docker rm -f bench_v >/dev/null 2>&1

# ---- redis 7.4 default ----
echo "=== redis 7.4 (default) ==="
docker run -d --rm --name bench_r --network host --cpuset-cpus "$SRV_CORES" \
  redis:7.4 redis-server --port 7003 --save '' --appendonly no >/dev/null 2>&1
run_two 7003 "redis-def"
docker rm -f bench_r >/dev/null 2>&1

# ---- redis 7.4 io-threads ----
echo "=== redis 7.4 (io-threads=$KEVY_THREADS) ==="
docker run -d --rm --name bench_r --network host --cpuset-cpus "$SRV_CORES" \
  redis:7.4 redis-server --port 7003 --save '' --appendonly no \
  --io-threads "$KEVY_THREADS" >/dev/null 2>&1
run_two 7003 "redis-iot"
docker rm -f bench_r >/dev/null 2>&1

echo "=== ALL DONE ==="
