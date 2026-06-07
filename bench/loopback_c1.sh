#!/usr/bin/env bash
# Clean -c1 -P1 (single connection, no pipeline => pure round-trip latency /
# throughput) for kevy (epoll + io_uring) vs the strongest valkey/redis configs.
# Isolated runs, server cores 0-9, client core 10 (one client core is plenty for
# one connection). host-loopback, in-memory.
set -u
cd "$(dirname "$0")"
SRV_CORES=${SRV_CORES:-0-9}
CLI_CORE=${CLI_CORE:-10}
N=${N:-300000}
KEVY_THREADS=${KEVY_THREADS:-10}
KBIN=${KBIN:-/root/kevy/target/release/kevy}

wait_ready() {
  for _ in $(seq 1 100); do
    redis-benchmark -h 127.0.0.1 -p "$1" -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.1
  done; echo "!! $1 down"; return 1
}
bench() { # port label
  local raw t v
  raw=$(taskset -c "$CLI_CORE" redis-benchmark -h 127.0.0.1 -p "$1" -t get,set \
    -n "$N" -c 1 -P 1 -q 2>/dev/null)
  for t in GET SET; do
    v=$(printf '%s' "$raw" | tr '\r' '\n' | grep "^$t: rps=" \
      | tail -1 | sed -E 's/^[A-Z]+: rps=[0-9.]+ \(overall: ([0-9.]+)\).*/\1/')
    printf '[%s] %s rps=%s\n' "$2" "$t" "$v"
  done
}
run() { wait_ready "$1" || return; redis-benchmark -h 127.0.0.1 -p "$1" -t set -n 50000 -q >/dev/null 2>&1; bench "$1" "$2"; }

echo "### -c1 -P1  n=$N  server=$SRV_CORES client=$CLI_CORE"
echo "=== kevy epoll ==="
KEVY_IO_URING=0 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$KEVY_THREADS" --port 7001 --no-aof >/tmp/c1_e.log 2>&1 &
P=$!; run 7001 "kevy-epoll"; kill "$P" 2>/dev/null; wait "$P" 2>/dev/null

echo "=== kevy io_uring ==="
KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$KEVY_THREADS" --port 7001 --no-aof >/tmp/c1_u.log 2>&1 &
P=$!; run 7001 "kevy-uring"; kill "$P" 2>/dev/null; wait "$P" 2>/dev/null

echo "=== valkey 9.1 (io-threads) ==="
docker run -d --rm --name bench_v --network host --cpuset-cpus "$SRV_CORES" \
  valkey/valkey:9.1 valkey-server --port 7002 --save '' --appendonly no --io-threads "$KEVY_THREADS" >/dev/null 2>&1
run 7002 "valkey-iot"; docker rm -f bench_v >/dev/null 2>&1

echo "=== valkey 9.1 (default) ==="
docker run -d --rm --name bench_v --network host --cpuset-cpus "$SRV_CORES" \
  valkey/valkey:9.1 valkey-server --port 7002 --save '' --appendonly no >/dev/null 2>&1
run 7002 "valkey-def"; docker rm -f bench_v >/dev/null 2>&1

echo "=== redis 7.4 (default) ==="
docker run -d --rm --name bench_r --network host --cpuset-cpus "$SRV_CORES" \
  redis:7.4 redis-server --port 7003 --save '' --appendonly no >/dev/null 2>&1
run 7003 "redis-def"; docker rm -f bench_r >/dev/null 2>&1
echo "### C1_DONE"
