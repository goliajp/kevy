#!/bin/bash
set -u
KBIN=/root/kevy/target/release/kevy
KPB=/root/kevy/target/release/kevy-pubsub-bench
VBIN=/root/srcbench/valkey/src/valkey-server
RBIN=/root/srcbench/redis/src/redis-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000
kill_all() { pkill -9 -f "/root/(kevy|srcbench)/(target|src/valkey|src/redis)" 2>/dev/null; sleep 1.5; }
wait_ready() { for _ in $(seq 1 30); do "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }
start_kevy(){ kill_all; setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$KBIN" --threads 1 --port $PORT --no-aof </dev/null >/tmp/k.log 2>&1 & disown; sleep 1; wait_ready; }
start_valkey(){ kill_all; setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes </dev/null >/tmp/v.log 2>&1 & disown; sleep 1.5; wait_ready; }
start_redis(){ kill_all; setsid taskset -c 0-9 "$RBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes </dev/null >/tmp/r.log 2>&1 & disown; sleep 1.5; wait_ready; }

run_pubsub() {
  local label=$1 subs=$2 msgs=$3 size=$4
  declare -a s=()
  for _ in 1 2 3; do
    r=$(timeout 60 taskset -c 10-13 "$KPB" --host 127.0.0.1 --port $PORT --subs $subs --msgs $msgs --size $size 2>&1 | grep -oE 'delivered=[0-9]+' | head -1 | cut -d= -f2)
    s+=("${r:-0}")
  done
  printf "%s subs=%-4d msgs=%-7d size=%-5d  median=%s\n" "$label" "$subs" "$msgs" "$size" "$(printf '%s\n' "${s[@]}" | sort -n | awk 'NR==2{print}')"
}

echo "=== Axis H quick re-bench post H1.A ==="
for srv in kevy valkey redis; do
  start_${srv}
  for spec in "10 100000 16" "50 100000 16" "100 50000 16" "200 20000 16" "50 50000 256" "50 20000 4096"; do
    read subs msgs size <<<"$spec"
    run_pubsub "$srv" $subs $msgs $size
  done
done
kill_all
