#!/usr/bin/env bash
# 6-way pub/sub bench driver: kevy / valkey / redis / zmq / aeron / zenoh.
#
# Same load shape across all six (50 subscribers, 200 k publishes, 16 B
# payload, 2 runs per backend). Reports `delivered msg/s` (= publishes ×
# subs / elapsed).
#
# Prereq: `docker compose --profile build build`
#
# Usage:
#     bash run.sh [SUBS=50] [MSGS=200000] [SIZE=16]

set -u
cd "$(dirname "$0")"

SUBS=${SUBS:-50}
MSGS=${MSGS:-200000}
SIZE=${SIZE:-16}

KEVY_PORT=7001
VALKEY_PORT=7002
REDIS_PORT=7003

COMPOSE="docker compose"

ensure_loadgen() {
  local id
  id=$($COMPOSE ps -q loadgen)
  if [ -z "$id" ] || ! docker inspect -f '{{.State.Running}}' "$id" 2>/dev/null | grep -q true; then
    $COMPOSE --profile loadgen up -d loadgen >/dev/null
    sleep 1
  fi
}

wait_redis_proto() { # host port
  for _ in $(seq 1 100); do
    docker compose exec -T loadgen redis-cli -h "$1" -p "$2" PING 2>/dev/null | grep -q PONG && return 0
    sleep 0.1
  done
  echo "  ERROR: port $2 did not answer PING" >&2
  return 1
}

bench_redis_proto() { # port label
  for _ in 1 2; do
    docker compose exec -T loadgen /usr/local/bin/kevy-pubsub-bench \
      --host 127.0.0.1 --port "$1" --subs "$SUBS" --msgs "$MSGS" --size "$SIZE" \
      | sed "s/^/[$2] /"
  done
}

bench_zmq() {
  for _ in 1 2; do
    docker compose exec -T \
      -e SUBS="$SUBS" -e MSGS="$MSGS" -e SIZE="$SIZE" \
      loadgen /usr/local/bin/zmq_pubsub_bench \
      | sed "s/^/[zmq] /"
  done
}

bench_zenoh() {
  for _ in 1 2; do
    docker compose exec -T \
      -e SUBS="$SUBS" -e MSGS="$MSGS" -e SIZE="$SIZE" \
      loadgen /usr/local/bin/zenoh_pubsub \
      | sed "s/^/[zenoh] /"
  done
}

bench_aeron() {
  # Clean previous driver shm if any.
  docker compose exec -T loadgen sh -c "rm -rf /dev/shm/aeron-* 2>/dev/null"
  for _ in 1 2; do
    docker compose exec -T \
      -e SUBS="$SUBS" -e MSGS="$MSGS" -e SIZE="$SIZE" \
      loadgen /usr/local/bin/aeron_pubsub \
      | sed "s/^/[aeron] /"
    docker compose exec -T loadgen sh -c "rm -rf /dev/shm/aeron-* 2>/dev/null"
  done
}

echo "### 6-way pub/sub bench   SUBS=$SUBS MSGS=$MSGS SIZE=${SIZE}B"
echo "### load: $(uptime)"
ensure_loadgen

# ── broker-based (RESP) ──
for sys in kevy valkey redis; do
  case $sys in
    kevy)   port=$KEVY_PORT ;;
    valkey) port=$VALKEY_PORT ;;
    redis)  port=$REDIS_PORT ;;
  esac
  echo "=== $sys ==="
  $COMPOSE --profile "$sys" up -d "$sys" >/dev/null
  wait_redis_proto 127.0.0.1 "$port" && bench_redis_proto "$port" "$sys"
  $COMPOSE --profile "$sys" stop "$sys" >/dev/null
  $COMPOSE --profile "$sys" rm -f "$sys" >/dev/null 2>&1
done

# ── direct-messaging (no broker) ──
echo "=== zmq ==="
bench_zmq

echo "=== zenoh ==="
bench_zenoh

echo "=== aeron (IPC) ==="
bench_aeron

echo "### tearing down loadgen"
$COMPOSE --profile loadgen stop loadgen >/dev/null
$COMPOSE --profile loadgen rm -f loadgen >/dev/null 2>&1
echo "### DONE"
