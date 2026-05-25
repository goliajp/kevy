#!/usr/bin/env bash
# Baseline benchmark: valkey 9.1 vs kevy (current single-connection blocking
# server), both in Docker Linux, measured by valkey-benchmark.
set -euo pipefail
cd "$(dirname "$0")"

N=${N:-200000}
TESTS=${TESTS:-ping,set,get,incr}

echo "### Bringing up valkey 9.1 + kevy (building kevy --release) ..."
docker compose up -d --build

wait_ping() { # host
  local host=$1 i
  for i in $(seq 1 60); do
    if docker compose exec -T loadgen valkey-cli -h "$host" -p 6379 ping 2>/dev/null | grep -q PONG; then
      echo "  $host: PONG"
      return 0
    fi
  done
  echo "  ERROR: $host did not answer PING" >&2
  return 1
}
echo "### Waiting for readiness ..."
wait_ping valkey
wait_ping kevy

bench() { # host concurrency — runs on the isolated loadgen cores
  docker compose exec -T loadgen valkey-benchmark -h "$1" -p 6379 -n "$N" -c "$2" -t "$TESTS" -q
}

echo
echo "=== valkey 9.1   -c 1   (n=$N, no pipeline) ==="
bench valkey 1
echo
echo "=== kevy         -c 1   (n=$N, no pipeline) ==="
bench kevy 1
echo
echo "=== valkey 9.1   -c 50  (n=$N) ==="
bench valkey 50
echo
echo "=== kevy         -c 50  (n=$N)  [now multiplexed by the reactor] ==="
bench kevy 50

echo
echo "### kevy startup (shard count):"
docker compose logs --no-log-prefix kevy 2>&1 | grep -i "starting" | head -1

echo
echo "### Tearing down ..."
docker compose down
