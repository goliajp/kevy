#!/bin/bash
# G3 bench: dispatch.rs F3 maxmemory hoist + util.rs F2' first-byte guard
# Target: c50-P1 SET/GET, c1-P1 SET/GET, axis C churn (-r 1M)
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
  local args="$@"
  declare -a s=()
  for _ in 1 2 3; do
    v=$(timeout 60 taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT $args -q 2>&1 | tr '\r' '\n' | grep -E "^[A-Z]+: [0-9]" | tail -1 | awk '{print $2}')
    s+=("${v:-0}")
  done
  printf '%s\n' "${s[@]}" | sort -n | awk 'NR==2{print}'
}

echo "=== G3 sanity + axis C churn (3-run median) ==="
for srv in kevy valkey; do
  start_${srv}
  c1s=$(run3med -t set -c 1 -n 200000 -P 1)
  c1g=$(run3med -t get -c 1 -n 200000 -P 1)
  c50s=$(run3med -t set -c 50 -n 500000 -P 1)
  c50g=$(run3med -t get -c 50 -n 500000 -P 1)
  cP16=$(run3med -t set -c 50 -n 1000000 -P 16)
  printf "%-6s c1-P1 SET=%-8s GET=%-8s c50-P1 SET=%-8s GET=%-8s c50-P16 SET=%-8s\n" "$srv" "$c1s" "$c1g" "$c50s" "$c50g" "$cP16"
done

echo "=== axis C churn (random keys -r 1M) ==="
for srv in kevy valkey; do
  start_${srv}
  cset100k=$(run3med -t set -c 50 -n 500000 -P 1 -r 100000)
  cset1M=$(run3med -t set -c 50 -n 500000 -P 1 -r 1000000)
  cset10M=$(run3med -t set -c 50 -n 500000 -P 1 -r 10000000)
  printf "%-6s SET -r100k=%-8s -r1M=%-8s -r10M=%-8s\n" "$srv" "$cset100k" "$cset1M" "$cset10M"
done
kill_all
