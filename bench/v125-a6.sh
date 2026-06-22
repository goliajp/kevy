#!/bin/bash
# A.6 bench: fuse get_for_reply into store-writes-direct path.
# Deco gain ≈ -5-8 ns/GET (sub-noise at c=50; visible at c=1 if at all).
# Bench validates: no regression on GET hot paths.
set -u
KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000
kill_all() { pkill -9 -f "/root/(kevy|srcbench)" 2>/dev/null; sleep 1.5; }
wait_ready() { for _ in $(seq 1 30); do "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }
start_kevy(){ kill_all; setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$KBIN" --threads 1 --port $PORT --no-aof </dev/null >/tmp/k.log 2>&1 & disown; sleep 1; wait_ready; }
start_valkey(){ kill_all; setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes </dev/null >/tmp/v.log 2>&1 & disown; sleep 1.5; wait_ready; }

med5() {
  local op=$1 c=$2 p=$3 n=$4
  declare -a s=()
  for _ in 1 2 3 4 5; do
    v=$(timeout 60 taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT -t "${op,,}" -c $c -P $p -n $n -q 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
    s+=("${v:-0}")
  done
  printf '%s\n' "${s[@]}" | sort -n | awk 'NR==3{print}'
}

echo "=== A.6 GET path validation (5-run median) ==="
echo "Pre-warm SET before each GET to populate"
for srv in kevy valkey; do
  start_${srv}
  taskset -c 10 "$RB" -h 127.0.0.1 -p $PORT -t set -n 50000 -q >/dev/null 2>&1
  c1g=$(med5 GET 1 1 300000)
  c50g=$(med5 GET 50 1 1000000)
  c100g=$(med5 GET 100 1 1000000)
  cP16g=$(med5 GET 50 16 1000000)
  c10kg=$(med5 GET 50 1 100000)
  printf "%s c1-P1=%-8s c50-P1=%-8s c100-P1=%-8s c50-P16=%-9s c50-10K=%-8s\n" \
    "$srv" "$c1g" "$c50g" "$c100g" "$cP16g" "$c10kg"
done
kill_all
