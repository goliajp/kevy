#!/bin/bash
# Axis I — tail latency (p50 / p95 / p99 / p99.9 ms)
# kevy --threads 1 vs valkey 9.1 vs redis 8.8
set -u

KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RBIN=/root/srcbench/redis/src/redis-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 65536

kill_all() { pkill -9 -f "/root/(kevy|srcbench)" 2>/dev/null; sleep 1; }

wait_ready() {
  for _ in $(seq 1 30); do
    "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_kevy() {
  kill_all
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$KBIN" --threads 1 --port $PORT --no-aof < /dev/null >/tmp/kevy.log 2>&1 &
  disown; sleep 1; wait_ready
}
start_valkey() {
  kill_all
  setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes < /dev/null >/tmp/valkey.log 2>&1 &
  disown; sleep 1; wait_ready
}
start_redis() {
  kill_all
  setsid taskset -c 0-9 "$RBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes < /dev/null >/tmp/redis.log 2>&1 &
  disown; sleep 1; wait_ready
}

# Parse percentile from full redis-benchmark output. Args: file, percentile (e.g. 50.000, 99.219, 99.902)
get_pct() {
  local file=$1 pct=$2
  grep -F "${pct}% <=" "$file" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}'
}

# Parse closest available percentile to target. redis-benchmark uses HDR-style
# 50, 75, 87.5, 93.75, 96.875, 98.438, 99.219, 99.609, 99.805, 99.902, 99.951...
run_scenario() {
  local label=$1 op=$2 cli_cores=$3 n=$4 c=$5 p=$6 size=$7
  printf "%-28s %s\n" "scenario=$label" "op=$op"
  for srv_cfg in "kevy:start_kevy" "valkey:start_valkey" "redis:start_redis"; do
    name="${srv_cfg%:*}"; fn="${srv_cfg#*:}"
    $fn || { printf "  %-7s FAIL\n" "$name"; continue; }
    sleep 1
    local tmpf=$(mktemp)
    local args=(-h 127.0.0.1 -p $PORT -t "${op,,}" -n $n -c $c -P $p --precision 3)
    [ "$size" != "0" ] && args+=(-d $size)
    taskset -c $cli_cores "$RB" "${args[@]}" 2>&1 | tr '\r' '\n' > "$tmpf"
    # Extract rps (last line of "OP: rps=..." progress, or "200000 requests completed in X seconds")
    local rps=$(grep -E "^${op}: [0-9]" "$tmpf" | tail -1 | awk '{print $2}')
    local p50=$(get_pct "$tmpf" "50.000")
    local p95=$(get_pct "$tmpf" "93.750")   # closest below 95 in redis-benchmark
    local p99=$(get_pct "$tmpf" "99.219")
    local p999=$(get_pct "$tmpf" "99.902")
    local pmax=$(grep -E "100.000% <=" "$tmpf" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
    printf "  %-7s rps=%-10s p50=%-7s p95~=%-7s p99~=%-7s p999~=%-7s max=%-7s\n" \
      "$name" "${rps:-0}" "${p50:-NA}" "${p95:-NA}" "${p99:-NA}" "${p999:-NA}" "${pmax:-NA}"
    rm -f "$tmpf"
  done
}

echo "=== Axis I — tail latency (ms) — kevy --threads 1 vs valkey 9.1 vs redis 8.8 ==="
echo "redis-benchmark percentiles: p50=50.000% p95~=93.750% p99~=99.219% p999~=99.902% max=100.000%"
echo ""

run_scenario "c1-P1"      SET 10    300000 1   1 0
run_scenario "c1-P1"      GET 10    300000 1   1 0
run_scenario "c50-P1"     SET 10-13 500000 50  1 0
run_scenario "c50-P1"     GET 10-13 500000 50  1 0
run_scenario "c100-P1"    SET 10-13 500000 100 1 0
run_scenario "c50-P16"    SET 10-13 1000000 50 16 0
run_scenario "c50-10KB"   SET 10-13 200000 50  1 10240
kill_all
