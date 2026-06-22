#!/bin/bash
# v1.25 A.2 — threshold sweep
# Compare A.2 with HEAP_HEAVY_BYTES ∈ {512, 1024, 4096} against the
# A.3 baseline (16K = old `pre-A.2`). 3-run median for sweep
# (variance budget — full run is the 5-run version in v125-a2.sh).
set -u

KBINS=(
  "pre-A.2(16K)::/root/kevy-a2-pre.bin"
  "post-A.2(512)::/root/kevy-a2-t512.bin"
  "post-A.2(1K)::/root/kevy-a2-post.bin"
  "post-A.2(4K)::/root/kevy-a2-t4k.bin"
)
VBIN=/root/srcbench/valkey/src/valkey-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000

kill_all() {
  pkill -9 -x kevy 2>/dev/null
  pkill -9 -x kevy-a2-pre.bin 2>/dev/null
  pkill -9 -x kevy-a2-post.bin 2>/dev/null
  pkill -9 -x kevy-a2-t512.bin 2>/dev/null
  pkill -9 -x kevy-a2-t4k.bin 2>/dev/null
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

start_kevy_bin() {
  local bin=$1
  kill_all
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$bin" --threads 1 --port $PORT --no-aof < /dev/null >/tmp/kevy.log 2>&1 &
  disown; sleep 1; wait_ready
}

get_pct() {
  local file=$1 pct=$2
  grep -F "${pct}% <=" "$file" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}'
}

median() {
  local arr=("$@")
  local n=${#arr[@]}
  if [ $n -eq 0 ]; then echo "NA"; return; fi
  printf "%s\n" "${arr[@]}" | sort -n | awk -v n=$n '{a[NR]=$1} END{ if (n%2==1) print a[(n+1)/2]; else printf "%.3f\n", (a[n/2]+a[n/2+1])/2 }'
}

single_run() {
  local op=$1 c=$2 p=$3 size=$4 n=$5 cli_cores=$6
  local tmpf=$(mktemp)
  local args=(-h 127.0.0.1 -p $PORT -t "${op,,}" -n $n -c $c -P $p --precision 3)
  [ "$size" != "0" ] && args+=(-d $size)
  timeout 90 taskset -c $cli_cores "$RB" "${args[@]}" 2>&1 | tr '\r' '\n' > "$tmpf"
  local p50=$(get_pct "$tmpf" "50.000")
  local p99=$(get_pct "$tmpf" "99.219")
  local p999=$(get_pct "$tmpf" "99.902")
  local pmax=$(grep -E "100.000% <=" "$tmpf" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
  rm -f "$tmpf"
  echo "${p50:-NA} ${p99:-NA} ${p999:-NA} ${pmax:-NA}"
}

run_median3() {
  local label=$1 op=$2 c=$3 p=$4 size=$5 n=$6 cli_cores=$7
  local p50_arr=() p99_arr=() p999_arr=() pmax_arr=()
  for i in 1 2 3; do
    read p50 p99 p999 pmax <<<"$(single_run $op $c $p $size $n $cli_cores)"
    p50_arr+=("$p50"); p99_arr+=("$p99")
    p999_arr+=("$p999"); pmax_arr+=("$pmax")
  done
  printf "  %-16s p50=%-7s p99=%-7s p999=%-7s max=%-7s\n" \
    "$label" \
    "$(median ${p50_arr[@]})" \
    "$(median ${p99_arr[@]})" "$(median ${p999_arr[@]})" \
    "$(median ${pmax_arr[@]})"
}

echo "=== v1.25 A.2 — threshold sweep ==="
echo "Bench: c=50 P=1 SET, n=200_000, 3-run median, --precision 3"
echo ""

for SIZE in 1024 4096 10240 65536; do
  echo "--- SET -d $SIZE ---"
  for entry in "${KBINS[@]}"; do
    local_label=${entry%%::*}
    local_bin=${entry##*::}
    start_kevy_bin "$local_bin"
    run_median3 "$local_label" SET 50 1 $SIZE 200000 5-9
  done
  echo ""
done

kill_all
echo "done."
