#!/bin/bash
# Post-all-attacks final verification:
#  - Axis I tail (c=50 -d 10240 SET p99/p999/max — A.3 bio thread should drop tail)
#  - Axis H pub/sub (subs sweep — G5 H1/H2 + A.4 writev-chunking + #109 routing shouldn't regress)
set -u
KBIN=/root/kevy/target/release/kevy
KPB=/root/kevy/target/release/kevy-pubsub-bench
VBIN=/root/srcbench/valkey/src/valkey-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000

kill_all() { pkill -9 -x kevy 2>/dev/null; pkill -9 -x valkey-server 2>/dev/null; sleep 2; }
wait_ready() { for _ in $(seq 1 30); do "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }

start_kevy(){ kill_all; setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$KBIN" --threads 1 --port $PORT --no-aof </dev/null >/tmp/k.log 2>&1 & disown; sleep 1; wait_ready; }
start_valkey(){ kill_all; setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes </dev/null >/tmp/v.log 2>&1 & disown; sleep 1.5; wait_ready; }

echo "=== Final verify: Axis I tail latency (c=50 -d 10240 SET, --precision 3, 3-run) ==="
for srv in kevy valkey; do
  start_${srv}
  for run in 1 2 3; do
    f=$(mktemp)
    timeout 90 taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT -t set -c 50 -P 1 -d 10240 -n 200000 --precision 3 2>&1 | tr '\r' '\n' > "$f"
    rps=$(grep -E "^SET: [0-9]" "$f" | tail -1 | awk '{print $2}')
    p50=$(grep -F "50.000% <=" "$f" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
    p99=$(grep -F "99.219% <=" "$f" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
    p999=$(grep -F "99.902% <=" "$f" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
    pmax=$(grep -E "^100.000% <=" "$f" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
    printf "  %s run=%d rps=%-9s p50=%-7s p99=%-7s p999=%-7s max=%-7s\n" "$srv" "$run" "${rps:-NA}" "${p50:-NA}" "${p99:-NA}" "${p999:-NA}" "${pmax:-NA}"
    rm -f "$f"
  done
done

echo ""
echo "=== Final verify: Axis H pub/sub sweep (3-run median) ==="
for srv in kevy valkey; do
  start_${srv}
  for spec in "10 100000 16" "50 100000 16" "100 50000 16" "200 20000 16" "500 10000 16" "50 50000 256" "50 20000 4096"; do
    read subs msgs size <<<"$spec"
    declare -a s=()
    for _ in 1 2 3; do
      r=$(timeout 90 taskset -c 10-13 "$KPB" --host 127.0.0.1 --port $PORT --subs $subs --msgs $msgs --size $size 2>&1 | grep -oE 'delivered=[0-9]+' | head -1 | cut -d= -f2)
      s+=("${r:-0}")
    done
    med=$(printf '%s\n' "${s[@]}" | sort -n | awk 'NR==2')
    printf "  %s subs=%-4d msgs=%-7d size=%-5d  med=%s\n" "$srv" "$subs" "$msgs" "$size" "$med"
  done
done

kill_all
echo "=== verify done ==="
