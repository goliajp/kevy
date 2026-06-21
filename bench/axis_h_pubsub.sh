#!/bin/bash
# Axis H — pub/sub fan-out throughput
# kevy --threads 1 vs valkey 9.1 vs redis 8.8
set -u

KBIN=/root/kevy/target/release/kevy
KPB=/root/kevy/target/release/kevy-pubsub-bench
VBIN=/root/srcbench/valkey/src/valkey-server
RBIN=/root/srcbench/redis/src/redis-server
PORT=7001
ulimit -n 65536

kill_all() { pkill -9 -f "/root/(kevy|srcbench)" 2>/dev/null; sleep 1; }

wait_ready() {
  for _ in $(seq 1 30); do
    /root/srcbench/redis/src/redis-benchmark -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0
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

run_scenario() {
  local label=$1 subs=$2 msgs=$3 size=$4
  printf "scenario=%-30s " "$label"
  for srv_cfg in "kevy:start_kevy" "valkey:start_valkey" "redis:start_redis"; do
    name="${srv_cfg%:*}"; fn="${srv_cfg#*:}"
    $fn || { printf "%s=FAIL " "$name"; continue; }
    sleep 1
    # 3 runs, take median delivered msg/s
    declare -a deliv=()
    for _ in 1 2 3; do
      r=$(taskset -c 10-13 "$KPB" --host 127.0.0.1 --port $PORT --subs $subs --msgs $msgs --size $size 2>&1 | grep -oE 'delivered=[0-9]+' | head -1 | cut -d= -f2)
      deliv+=("${r:-0}")
    done
    med=$(printf "%s\n" "${deliv[@]}" | sort -n | awk 'NR==2{print}')
    printf "%s=%-12s " "$name" "$med"
  done
  printf "\n"
}

echo "=== Axis H — pub/sub fan-out (delivered msg/s, median of 3) ==="
echo "host=lx64 kevy --threads 1 vs valkey 9.1 vs redis 8.8"
run_scenario "subs=10  msgs=100000 size=16"  10  100000 16
run_scenario "subs=50  msgs=100000 size=16"  50  100000 16
run_scenario "subs=100 msgs=50000  size=16"  100 50000  16
run_scenario "subs=200 msgs=20000  size=16"  200 20000  16
run_scenario "subs=500 msgs=10000  size=16"  500 10000  16
run_scenario "subs=50  msgs=50000  size=256" 50  50000  256
run_scenario "subs=50  msgs=20000  size=4096" 50 20000  4096
kill_all
