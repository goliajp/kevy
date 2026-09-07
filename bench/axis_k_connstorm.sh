#!/bin/bash
# Axis K — connection storm (c >> 2000)
# kevy --threads 1 vs valkey and redis, at the versions COMPETITOR-ANCHORS.json pins
set -u

KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RBIN=/root/srcbench/redis/src/redis-server
RB=/root/srcbench/redis/src/redis-benchmark

# The source-built competitors this probe measures against must be the
# versions on record; a probe that quietly runs an older redis produces a
# number nobody can place. See bench/anchor-lib.sh.
. "$(dirname "$0")/anchor-lib.sh"
command -v anchor_pin >/dev/null || { echo "$(basename "$0"): anchor-lib.sh did not load" >&2; exit 2; }
anchor_require "redis (source build)" "$(anchor_pin redis)" "$(anchor_bin_ver "$RBIN")"
anchor_require "valkey (source build)" "$(anchor_pin valkey)" "$(anchor_bin_ver "$VBIN")"

PORT=7001
ulimit -n 200000

kill_all() { pkill -9 -f "/root/(kevy|srcbench)" 2>/dev/null; sleep 2; }

wait_ready() {
  for _ in $(seq 1 30); do
    "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_kevy_t() {
  # arg: thread count
  local t=$1
  kill_all
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "0-$((t-1))" "$KBIN" --threads $t --port $PORT --no-aof < /dev/null >/tmp/k.log 2>&1 &
  disown; sleep 1; wait_ready
}
start_valkey() {
  kill_all
  setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes < /dev/null >/tmp/v.log 2>&1 &
  disown; sleep 2; wait_ready
}
start_redis() {
  kill_all
  setsid taskset -c 0-9 "$RBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes < /dev/null >/tmp/r.log 2>&1 &
  disown; sleep 2; wait_ready
}

# Args: op, c, n
run_op() {
  local op=$1 c=$2 n=$3
  declare -a samples=()
  for _ in 1 2; do
    v=$(taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT -t "${op,,}" -c $c -P 1 -n $n -q 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
    samples+=("${v:-0}")
  done
  printf '%s\n' "${samples[@]}" | sort -n | tail -1
}

echo "=== Axis K — connection storm (rps, best of 2) ==="
echo "kevy with t=1, t=2, t=4 sweep; vs valkey (10 io_threads); vs redis (10 io_threads)"
echo ""
for c in 3000 5000 8000 10000; do
  n=2000000
  echo "--- c=$c (n=$n) ---"
  for t in 1 2 4; do
    start_kevy_t $t || { echo "  kevy t=$t FAIL"; continue; }
    k_set=$(run_op SET $c $n)
    k_get=$(run_op GET $c $n)
    printf "  kevy t=%d  SET=%-10s  GET=%-10s\n" "$t" "$k_set" "$k_get"
  done
  start_valkey || { echo "  valkey FAIL"; continue; }
  v_set=$(run_op SET $c $n)
  v_get=$(run_op GET $c $n)
  printf "  valkey    SET=%-10s  GET=%-10s\n" "$v_set" "$v_get"
  start_redis  || { echo "  redis FAIL"; continue; }
  r_set=$(run_op SET $c $n)
  r_get=$(run_op GET $c $n)
  printf "  redis     SET=%-10s  GET=%-10s\n" "$r_set" "$r_get"
done
kill_all
