#!/bin/bash
# v1.25 A.3 — stretch test: confirm bio thread actually carries work
# at value sizes ≥ HEAP_HEAVY_BYTES (16 KB).
#
# At 32 KB and 64 KB, the overwrite Drop of `Value::ArcBulk` would
# normally stall the reactor on jemalloc large-class madvise; with
# bio-thread off-load, the reactor's tail should be lower than the
# inline-drop equivalent.
#
# This is the "the feature actually fires" smoke test, complementary
# to v125-a3.sh which holds the standard 10 KB shape.
set -u

KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000

kill_all() {
  pkill -9 -x kevy 2>/dev/null
  local pids
  pids=$(lsof -t -iTCP:$PORT -sTCP:LISTEN 2>/dev/null)
  [ -n "$pids" ] && kill -9 $pids 2>/dev/null
  sleep 1
}

wait_ready() {
  for _ in $(seq 1 30); do
    "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_kevy()  { kill_all; setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$KBIN" --threads 1 --port $PORT --no-aof < /dev/null >/tmp/kevy.log 2>&1 & disown; sleep 1; wait_ready; }
start_valkey(){ kill_all; setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes < /dev/null >/tmp/valkey.log 2>&1 & disown; sleep 1; wait_ready; }

get_pct() { grep -F "${2}% <=" "$1" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}'; }

median() {
  local arr=("$@") n=${#arr[@]}
  if [ $n -eq 0 ]; then echo "NA"; return; fi
  printf "%s\n" "${arr[@]}" | sort -n | awk -v n=$n '{a[NR]=$1} END{ if (n%2==1) print a[(n+1)/2]; else printf "%.3f\n", (a[n/2]+a[n/2+1])/2 }'
}

single_run() {
  local op=$1 c=$2 p=$3 size=$4 n=$5 cli_cores=$6
  local tmpf=$(mktemp)
  local args=(-h 127.0.0.1 -p $PORT -t "${op,,}" -n $n -c $c -P $p --precision 3)
  [ "$size" != "0" ] && args+=(-d $size)
  timeout 90 taskset -c $cli_cores "$RB" "${args[@]}" 2>&1 | tr '\r' '\n' > "$tmpf"
  local p99=$(get_pct "$tmpf" "99.219")
  local p999=$(get_pct "$tmpf" "99.902")
  local pmax=$(grep -E "100.000% <=" "$tmpf" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
  rm -f "$tmpf"
  echo "${p99:-NA} ${p999:-NA} ${pmax:-NA}"
}

run_median5() {
  local label=$1 op=$2 c=$3 p=$4 size=$5 n=$6 cli_cores=$7
  local p99_arr=() p999_arr=() pmax_arr=()
  for _ in 1 2 3 4 5; do
    read p99 p999 pmax <<<"$(single_run $op $c $p $size $n $cli_cores)"
    p99_arr+=("$p99"); p999_arr+=("$p999"); pmax_arr+=("$pmax")
  done
  printf "  %-7s p99=%-7s p999=%-7s max=%-7s\n" \
    "$label" "$(median ${p99_arr[@]})" "$(median ${p999_arr[@]})" "$(median ${pmax_arr[@]})"
}

echo "=== v1.25 A.3 stretch: ≥ HEAP_HEAVY_BYTES (16 KB) — bio actually fires ==="
echo ""

echo "--- 32 KB SET overwrite churn ---"
start_kevy && run_median5 "kevy"  SET 50 1 32768 100000 10-13
start_valkey && run_median5 "valkey" SET 50 1 32768 100000 10-13

echo ""
echo "--- 64 KB SET overwrite churn ---"
start_kevy && run_median5 "kevy"  SET 50 1 65536 50000 10-13
start_valkey && run_median5 "valkey" SET 50 1 65536 50000 10-13

kill_all
echo ""
echo "=== stretch bench complete ==="
