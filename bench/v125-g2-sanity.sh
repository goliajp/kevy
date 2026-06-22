#!/bin/bash
# G2 sanity — 3-run median of the small-value scenarios that may have regressed
set -u
KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000
kill_all() { pkill -9 -f "/root/(kevy|srcbench)/(target|src/valkey|src/redis)" 2>/dev/null; sleep 1.5; }
wait_ready() { for _ in $(seq 1 30); do "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }
start_kevy(){ kill_all; setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$KBIN" --threads 1 --port $PORT --no-aof </dev/null >/tmp/k.log 2>&1 & disown; sleep 1; wait_ready; }
start_valkey(){ kill_all; setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes </dev/null >/tmp/v.log 2>&1 & disown; sleep 1.5; wait_ready; }

run3med() {
  local op=$1 c=$2 n=$3
  declare -a s=()
  for _ in 1 2 3; do
    v=$(timeout 60 taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT -t "${op,,}" -c $c -n $n -P 1 -q 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
    s+=("${v:-0}")
  done
  printf '%s\n' "${s[@]}" | sort -n | awk 'NR==2{print}'
}

echo "=== 3-run median sanity ==="
for srv in kevy valkey; do
  start_${srv}
  c1s=$(run3med SET 1 200000)
  c1g=$(run3med GET 1 200000)
  c50s=$(run3med SET 50 500000)
  c50g=$(run3med GET 50 500000)
  printf "%s  c1-P1 SET=%-10s  GET=%-10s   c50-P1 SET=%-10s  GET=%-10s\n" "$srv" "$c1s" "$c1g" "$c50s" "$c50g"
done
kill_all
