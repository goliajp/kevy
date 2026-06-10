#!/usr/bin/env bash
# Portability / smoke harness: valkey 9.1 vs kevy via Docker bridge, default
# `-c50 -P1` valkey-benchmark. Exercises the protocol end-to-end so a fresh
# clone can verify "it runs and answers". NOT a perf benchmark — docker NAT
# softirq, no pipelining, and short N depress kevy's busy-poll path. For
# headline perf numbers use `bench/loopback_c50.sh` (host-loopback, pinned,
# isolated; see bench/REPORT.md).
set -euo pipefail
cd "$(dirname "$0")"

N=${N:-200000}
TESTS=${TESTS:-ping,set,get,incr}

cat <<'BANNER'
### NOTE: this is a portability smoke (docker-bridge, no pipeline). It is NOT
### the perf harness — for headline kevy-vs-valkey numbers run
###   bash bench/loopback_c50.sh   # host-loopback, pinned, isolated
### See bench/REPORT.md for the methodology and current numbers.
BANNER

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

cat <<'FOOTER'

### Reminder: this is a smoke run, not a perf measurement. docker-bridge NAT
### and `-P1` favour blocking servers and depress kevy's busy-poll path; on
### host-loopback with pinning + pipelining (bench/loopback_c50.sh) kevy
### currently leads valkey ~1.5×/2.0× GET/SET at -c50 -P16. See bench/REPORT.md.

FOOTER

echo "### Tearing down ..."
docker compose down
