#!/bin/bash
# G2 partial bench: A1 (parse-from-slab) + B-A3 (pre-grow) + epoll-output-arcs-fix
# Per-bench timeout 90s, always pkill between runs.
set -u
KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000

kill_all() { pkill -9 -f "/root/(kevy|srcbench)/(target|src/valkey-server|src/redis-server)" 2>/dev/null; sleep 1.5; }

wait_ready() {
  for _ in $(seq 1 30); do
    "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_kevy() {
  kill_all
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$KBIN" --threads 1 --port $PORT --no-aof < /dev/null >/tmp/k.log 2>&1 &
  disown; sleep 1; wait_ready
}
start_valkey() {
  kill_all
  setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes < /dev/null >/tmp/v.log 2>&1 &
  disown; sleep 1.5; wait_ready
}

# Args: op c n d
run_op_timed() {
  local op=$1 c=$2 n=$3 d=$4
  local args=(-h 127.0.0.1 -p $PORT -t "${op,,}" -c "$c" -n "$n" -P 1 -q)
  [ "$d" != "0" ] && args+=(-d "$d")
  v=$(timeout 90 taskset -c 10-13 "$RB" "${args[@]}" 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
  echo "${v:-TIMEOUT}"
}

# percentile extraction
run_lat_timed() {
  local op=$1 c=$2 n=$3 d=$4
  local args=(-h 127.0.0.1 -p $PORT -t "${op,,}" -c "$c" -n "$n" -P 1 --precision 3)
  [ "$d" != "0" ] && args+=(-d "$d")
  local f=$(mktemp)
  timeout 90 taskset -c 10-13 "$RB" "${args[@]}" 2>&1 | tr '\r' '\n' > "$f"
  local p50=$(grep -F "50.000% <=" "$f" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
  local p99=$(grep -F "99.219% <=" "$f" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
  local p999=$(grep -F "99.902% <=" "$f" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
  local pmax=$(grep -E "^100.000% <=" "$f" | head -1 | awk -F'<=' '{print $2}' | awk '{print $1}')
  rm -f "$f"
  printf "p50=%s p99=%s p999=%s max=%s\n" "${p50:-NA}" "${p99:-NA}" "${p999:-NA}" "${pmax:-NA}"
}

echo "=== G2 partial (A1 + B-A3 + epoll output_arcs fix) bench ==="
echo "Axis I primary: c=50 -d 10240 SET/GET"
for srv in kevy valkey; do
  start_${srv}
  r=$(run_op_timed SET 50 200000 10240)
  echo "  $srv c50-10K SET rps=$r"
  l=$(run_lat_timed SET 50 200000 10240)
  echo "  $srv c50-10K SET $l"
  r=$(run_op_timed GET 50 200000 10240)
  echo "  $srv c50-10K GET rps=$r"
  l=$(run_lat_timed GET 50 200000 10240)
  echo "  $srv c50-10K GET $l"
done
echo ""
echo "Axis B primary: c=50 -d 65536 SET/GET"
for srv in kevy valkey; do
  start_${srv}
  r=$(run_op_timed SET 50 50000 65536)
  echo "  $srv c50-64K SET rps=$r"
  r=$(run_op_timed GET 50 50000 65536)
  echo "  $srv c50-64K GET rps=$r"
done
echo ""
echo "Axis B boundary: c=50 -d 16384 SET/GET"
for srv in kevy valkey; do
  start_${srv}
  r=$(run_op_timed SET 50 100000 16384)
  echo "  $srv c50-16K SET rps=$r"
  r=$(run_op_timed GET 50 100000 16384)
  echo "  $srv c50-16K GET rps=$r"
done
echo ""
echo "=== sanity: no regression on existing wins ==="
for srv in kevy valkey; do
  start_${srv}
  s1=$(run_op_timed SET 1 300000 0)
  g1=$(run_op_timed GET 1 300000 0)
  s50=$(run_op_timed SET 50 1000000 0)
  g50=$(run_op_timed GET 50 1000000 0)
  echo "  $srv c1-P1: SET=$s1 GET=$g1   c50-P1: SET=$s50 GET=$g50"
done
echo "c=50 -P 16:"
for srv in kevy valkey; do
  start_${srv}
  s=$(timeout 60 taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT -t set -c 50 -P 16 -n 1000000 -q 2>&1 | tr '\r' '\n' | grep -E "^SET: [0-9]" | tail -1 | awk '{print $2}')
  echo "  $srv c50-P16 SET=$s"
done
kill_all
