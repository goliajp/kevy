#!/usr/bin/env bash
# Pub/Sub fan-out throughput: valkey 9.1 vs redis 7.4 vs kevy, measured by the
# pure-Rust kevy-pubsub-bench (valkey-benchmark has no pub/sub mode). K
# subscribers + a flooding publisher; headline = delivered messages/sec.
#
# Like bench3.sh: kevy busy-polls, so run on an idle host for clean numbers.
# Env: SUBS (subscribers), MSGS (publishes), SIZE (payload bytes), KEVY_IO_URING.
set -euo pipefail
cd "$(dirname "$0")"

SUBS=${SUBS:-50}
MSGS=${MSGS:-100000}
SIZE=${SIZE:-16}

echo "### host load: $(sysctl -n vm.loadavg 2>/dev/null || uptime)"
echo "### bringing up valkey + redis + kevy ..."
docker compose up -d --build valkey redis kevy loadgen >/dev/null 2>&1
for h in valkey redis kevy; do
  for _ in $(seq 1 90); do
    docker compose exec -T loadgen valkey-cli -h "$h" -p 6379 ping 2>/dev/null | grep -q PONG && break
    sleep 0.3
  done
done
echo "### kevy: $(docker compose logs --no-log-prefix kevy 2>&1 | grep -i 'starting' | head -1)"

# One throwaway rust container on the bench network builds the tool once and runs
# it against all three servers (ephemeral target dir, removed with --rm).
docker run --rm --network bench_bench \
  -v "$(cd .. && pwd)":/src -w /src -e CARGO_TARGET_DIR=/tmp/lt \
  rust:1.95-slim-bookworm \
  sh -c '
    cargo build -q -p kevy-loadgen --release
    for h in valkey redis kevy; do
      /tmp/lt/release/kevy-pubsub-bench --host "$h" --port 6379 \
        --subs '"$SUBS"' --msgs '"$MSGS"' --size '"$SIZE"'
    done
  '

echo "### tearing down ..."
docker compose down >/dev/null 2>&1
