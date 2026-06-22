#!/bin/bash
# v1.25 A.3 — bio-thread lazy-drop bench
#
# Target: Axis I 10 KB SET tail max — pre-attack ~130-160 ms (B.4 agent
# report). After attack, single-threaded sync Drop of overwritten
# Value::ArcBulk is moved to a global bio thread (B2 architecture,
# per bench/V125-DECISIONS-PENDING.md).
#
# Sanity: matrix-style standard scenarios — must NOT regress.
#
# Hygiene: `pkill -x` matches process NAME, not argv (the latter would
# also match THIS script). Per-bench timeout 90 s. 5-run median for
# tail metrics so jemalloc large-class madvise jitter doesn't fake a win.
set -u

KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000

# `-x` matches NAME only (the prompt's lesson from the a61652f3 zombie
# incident: `-f` over the full cmdline can match the bench script's
# own argv too).
#
# But other projects' valkey-server on :6379 must NOT be killed (cross-
# project shared host). So for valkey/redis we match by listen-port,
# falling back to nothing on Mac/etc. (lsof guarantees -t outputs
# nothing if no listener). pkill -x kevy is fine because no other
# project on lx64 runs a `kevy` named binary.
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

get_pct() {
  local file=$1 pct=$2
  grep -F "${pct}% <=" "$file" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}'
}

# Median of n floats — pure bash + sort -n
median() {
  local arr=("$@")
  local n=${#arr[@]}
  if [ $n -eq 0 ]; then echo "NA"; return; fi
  printf "%s\n" "${arr[@]}" | sort -n | awk -v n=$n '{a[NR]=$1} END{ if (n%2==1) print a[(n+1)/2]; else printf "%.3f\n", (a[n/2]+a[n/2+1])/2 }'
}

# Single bench run. Args: op c p size n cli_cores
single_run() {
  local op=$1 c=$2 p=$3 size=$4 n=$5 cli_cores=$6
  local tmpf=$(mktemp)
  local args=(-h 127.0.0.1 -p $PORT -t "${op,,}" -n $n -c $c -P $p --precision 3)
  [ "$size" != "0" ] && args+=(-d $size)
  timeout 90 taskset -c $cli_cores "$RB" "${args[@]}" 2>&1 | tr '\r' '\n' > "$tmpf"
  local rps=$(grep -E "^${op}: [0-9]" "$tmpf" | tail -1 | awk '{print $2}')
  local p50=$(get_pct "$tmpf" "50.000")
  local p99=$(get_pct "$tmpf" "99.219")
  local p999=$(get_pct "$tmpf" "99.902")
  local pmax=$(grep -E "100.000% <=" "$tmpf" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
  rm -f "$tmpf"
  echo "${rps:-0} ${p50:-NA} ${p99:-NA} ${p999:-NA} ${pmax:-NA}"
}

# Run 5 times against the SAME server instance; median each metric.
run_median5() {
  local label=$1 op=$2 c=$3 p=$4 size=$5 n=$6 cli_cores=$7
  local rps_arr=() p50_arr=() p99_arr=() p999_arr=() pmax_arr=()
  for i in 1 2 3 4 5; do
    read rps p50 p99 p999 pmax <<<"$(single_run $op $c $p $size $n $cli_cores)"
    rps_arr+=("$rps"); p50_arr+=("$p50"); p99_arr+=("$p99")
    p999_arr+=("$p999"); pmax_arr+=("$pmax")
  done
  printf "  %-7s rps=%-10s p50=%-7s p99=%-7s p999=%-7s max=%-7s\n" \
    "$label" \
    "$(median ${rps_arr[@]})" "$(median ${p50_arr[@]})" \
    "$(median ${p99_arr[@]})" "$(median ${p999_arr[@]})" \
    "$(median ${pmax_arr[@]})"
}

echo "=== v1.25 A.3 bio-thread lazy-drop bench (5-run median) ==="
echo "    pre-attack baseline (b.4 agent): Axis I 10 KB SET max ~130-160 ms"
echo "    target: drop tail max substantially; valkey ~1 ms is the upper bound"
echo ""

# ============ Axis I — THE killer metric (overwrite-heavy 10K SET) ============
echo "--- Axis I: c=50 P=1 10 KB SET (the lazy-drop target) ---"
start_kevy && run_median5 "kevy"  SET 50 1 10240 200000 10-13
start_valkey && run_median5 "valkey" SET 50 1 10240 200000 10-13

# Higher conc to stress overwrite churn more
echo ""
echo "--- Axis I extended: c=100 P=1 10 KB SET ---"
start_kevy && run_median5 "kevy"  SET 100 1 10240 200000 10-13
start_valkey && run_median5 "valkey" SET 100 1 10240 200000 10-13

# ============ Sanity matrix — must NOT regress ==============================
echo ""
echo "--- Sanity: c=50 P=1 SET (small value, no offload triggered) ---"
start_kevy && run_median5 "kevy"  SET 50 1 0 500000 10-13
start_valkey && run_median5 "valkey" SET 50 1 0 500000 10-13

echo ""
echo "--- Sanity: c=50 P=1 GET (read-only, offload path never enters) ---"
start_kevy && run_median5 "kevy"  GET 50 1 0 500000 10-13
start_valkey && run_median5 "valkey" GET 50 1 0 500000 10-13

echo ""
echo "--- Sanity: c=50 P=16 SET pipelined (the v1.25 headline) ---"
start_kevy && run_median5 "kevy"  SET 50 16 0 1000000 10-13
start_valkey && run_median5 "valkey" SET 50 16 0 1000000 10-13

echo ""
echo "--- Sanity: c=1 P=1 SET (latency floor) ---"
start_kevy && run_median5 "kevy"  SET 1 1 0 300000 10
start_valkey && run_median5 "valkey" SET 1 1 0 300000 10

kill_all
echo ""
echo "=== bench complete ==="
