#!/usr/bin/env bash
# 3-way throughput: valkey 9.1 vs redis 7.4 vs kevy (default epoll reactor),
# measured by the neutral valkey-benchmark on isolated load-gen cores. Both -c1
# (per-connection, apples-to-apples) and -c50 (concurrency). In-memory on all
# (no persistence). The ratio within a run is the signal; absolute rps is
# depressed by the macOS Docker VM (and by any host CPU contention — kevy
# busy-polls, so a busy host starves it; run on an idle box for clean numbers).
set -euo pipefail
cd "$(dirname "$0")"

N=${N:-200000}
TESTS=${TESTS:-ping,set,get,incr}

echo "### host load: $(sysctl -n vm.loadavg 2>/dev/null || uptime)"
echo "### bringing up valkey + redis + kevy (default epoll) ..."
docker compose up -d --build valkey redis kevy loadgen >/dev/null 2>&1
for h in valkey redis kevy; do
  for _ in $(seq 1 90); do
    docker compose exec -T loadgen valkey-cli -h "$h" -p 6379 ping 2>/dev/null | grep -q PONG && break
    sleep 0.3
  done
done
echo "### kevy: $(docker compose logs --no-log-prefix kevy 2>&1 | grep -i 'starting' | head -1)"

bench() { docker compose exec -T loadgen valkey-benchmark -h "$1" -p 6379 -n "$N" -c "$2" -t "$TESTS" -q; }

for c in 1 50; do
  for h in valkey redis kevy; do
    echo "=== $h  -c$c  (n=$N) ==="
    bench "$h" "$c"
  done
done

echo "### tearing down ..."
docker compose down >/dev/null 2>&1
