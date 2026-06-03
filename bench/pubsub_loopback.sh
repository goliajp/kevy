#!/usr/bin/env bash
# Clean pub/sub 3-way: K subscribers on one channel + a flooding publisher,
# measured by the pure-Rust kevy-pubsub-bench (no docker bridge / NAT — host
# loopback). Server pinned to cores 0-9, load generator to disjoint 10-15 so
# kevy's busy-poll can't starve it (the artifact that understated the earlier
# docker-bridge 2.28x). Each server isolated (start, bench, stop). In-memory.
set -u
cd "$(dirname "$0")"
SRV_CORES=${SRV_CORES:-0-9}
CLI_CORES=${CLI_CORES:-10-15}
SUBS=${SUBS:-50}
MSGS=${MSGS:-100000}
SIZE=${SIZE:-16}
THREADS=${KEVY_THREADS:-10}
KBIN=${KBIN:-/root/kevy/target/release/kevy}
LG=${LG:-/root/kevy/target/release/kevy-pubsub-bench}

wait_ready() { for _ in $(seq 1 100); do
  redis-benchmark -h 127.0.0.1 -p "$1" -t ping -c 1 -n 1 -q >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
run() { # port label
  wait_ready "$1" || { echo "[$2] down"; return; }
  for _ in 1 2; do
    taskset -c "$CLI_CORES" "$LG" --host 127.0.0.1 --port "$1" --subs "$SUBS" --msgs "$MSGS" --size "$SIZE" \
      | sed "s/^/[$2] /"
  done
}

echo "### pub/sub loopback  server=$SRV_CORES client=$CLI_CORES subs=$SUBS msgs=$MSGS size=${SIZE}B"
echo "### $(uptime)"

echo "=== kevy epoll (${THREADS}sh) ==="
KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$THREADS" --port 7001 --no-aof >/tmp/ps_e.log 2>&1 &
P=$!; run 7001 "kevy-epoll"; kill "$P" 2>/dev/null; wait "$P" 2>/dev/null

echo "=== kevy io_uring (${THREADS}sh) ==="
KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$THREADS" --port 7001 --no-aof >/tmp/ps_u.log 2>&1 &
P=$!; run 7001 "kevy-uring"; kill "$P" 2>/dev/null; wait "$P" 2>/dev/null

echo "=== valkey 9.1 ==="
docker run -d --rm --name bench_v --network host --cpuset-cpus "$SRV_CORES" \
  valkey/valkey:9.1 valkey-server --port 7002 --save '' --appendonly no >/dev/null 2>&1
run 7002 "valkey9.1"; docker rm -f bench_v >/dev/null 2>&1

echo "=== redis 7.4 ==="
docker run -d --rm --name bench_r --network host --cpuset-cpus "$SRV_CORES" \
  redis:7.4 redis-server --port 7003 --save '' --appendonly no >/dev/null 2>&1
run 7003 "redis7.4"; docker rm -f bench_r >/dev/null 2>&1
echo "### PUBSUB_DONE"
