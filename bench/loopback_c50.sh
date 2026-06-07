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
N=${N:-3000000}
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

bench() { # port label
  taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p "$1" \
    -t get,set -n "$N" -c "$CONC" -P "$PIPE" --threads "$CLI_THREADS" -q \
    2>/dev/null | grep -E "requests per second" | sed "s/^/[$2] /"
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
