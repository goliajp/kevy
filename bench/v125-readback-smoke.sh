#!/bin/bash
# Read-back semantic validation (R10) — verify writes actually land at the
# right shard before trusting any throughput number.
#
# bg: B.4 (commit 062eaa4) reported "Axis B 64 KiB SET 1.028× valkey" — was
# paper because bare-SET fast path bypassed cross-shard routing. ~15/16 SETs
# silently dropped in default 16-shard config. redis-benchmark -t set/get
# uses different keyspaces so it never validates that a SET key K can be
# GET'd back. This script does the SET-then-GET on the same K.
#
# Per-bench timeout per R1; pkill -9 -x kevy (not -f) per zombie-incident.
#
# Usage:
#   bash bench/v125-readback-smoke.sh [--threads N] [--size BYTES]
#
# Exits 0 if all written keys read back correctly, 1 otherwise.

set -u
KBIN=${KEVY_BIN:-/root/kevy/target/release/kevy}
RCLI=${REDIS_CLI:-/root/srcbench/redis/src/redis-cli}
PORT=${PORT:-7001}
THREADS=${THREADS:-1}
SIZE=${SIZE:-1024}
N_KEYS=${N_KEYS:-1000}

ulimit -n 200000

kill_all() {
  pkill -9 -x kevy 2>/dev/null
  sleep 1.5
}

cleanup() {
  kill_all
}
trap cleanup EXIT

kill_all

# Start kevy with given threads
CORES=$(seq -s, 0 $((THREADS - 1)))
setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 \
  taskset -c "$CORES" "$KBIN" --threads "$THREADS" --port "$PORT" --no-aof \
  </dev/null >/tmp/k.log 2>&1 &
disown
sleep 1

# Wait for kevy to be ready
for _ in $(seq 1 30); do
  "$RCLI" -p "$PORT" ping >/dev/null 2>&1 && break
  sleep 0.2
done

echo "=== read-back smoke (threads=$THREADS, size=$SIZE B, N_keys=$N_KEYS) ==="

# Generate distinct value per key (each value is $SIZE bytes of "${i}xxxx...")
mkfifo /tmp/v125-readback-pipe 2>/dev/null || true

# Phase 1: SET N_KEYS distinct keys, each with a distinct value
fail_set=0
for i in $(seq 1 "$N_KEYS"); do
  # 8-byte index prefix + filler so values are distinct AND deterministic
  val=$(printf "%08d" "$i")
  pad_len=$((SIZE - 8))
  pad=$(printf 'x%.0s' $(seq 1 $pad_len))
  full="${val}${pad}"
  if ! "$RCLI" -p "$PORT" SET "smoke:k:$i" "$full" >/dev/null 2>&1; then
    fail_set=$((fail_set + 1))
  fi
done

if [ "$fail_set" -ne 0 ]; then
  echo "FAIL: $fail_set SET calls errored"
  exit 1
fi

# Phase 2: GET back, verify byte-for-byte
fail_get=0
fail_value=0
for i in $(seq 1 "$N_KEYS"); do
  val=$(printf "%08d" "$i")
  pad_len=$((SIZE - 8))
  pad=$(printf 'x%.0s' $(seq 1 $pad_len))
  expected="${val}${pad}"
  actual=$("$RCLI" -p "$PORT" GET "smoke:k:$i" 2>/dev/null)
  if [ -z "$actual" ]; then
    fail_get=$((fail_get + 1))
  elif [ "$actual" != "$expected" ]; then
    fail_value=$((fail_value + 1))
  fi
done

if [ "$fail_get" -ne 0 ] || [ "$fail_value" -ne 0 ]; then
  echo "FAIL: missing=$fail_get / wrong-value=$fail_value out of $N_KEYS"
  echo "  (likely a routing bug — keys SET to one shard, looked up via wrong shard,"
  echo "   e.g. R10 trigger: B.4-style fast path bypassing CRC16 cross-shard hop)"
  exit 1
fi

echo "OK: $N_KEYS keys round-tripped at threads=$THREADS, size=$SIZE B"

# Phase 3: DBSIZE sanity (server confirms exactly N_KEYS in keyspace)
dbsize=$("$RCLI" -p "$PORT" DBSIZE 2>/dev/null)
if [ "$dbsize" != "$N_KEYS" ]; then
  echo "FAIL: DBSIZE=$dbsize expected $N_KEYS"
  exit 1
fi
echo "OK: DBSIZE=$dbsize matches"
echo "PASS"
