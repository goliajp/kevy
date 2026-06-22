#!/bin/bash
# K4 (v1.25 A.9) bench — ready-set bitmap arm-loop.
#
# Validates:
#  - c=10000 t=1 SET/GET: G1 baseline ~120 k → expect 160-200 k per Phase A deco
#  - c=8000  t=1 SET/GET: G1 baseline ~104 k → expect similar improvement
#  - sanity at small N (c=1, c=50): MUST NOT regress
#  - vs valkey at the same shapes
#
# Per R6: cumulative attack; per R7: 3-run median, n=1M.
# Per-bench timeout 90 s to surface hangs (e.g. cliff regression) fast.
set -u
KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RB=/root/srcbench/redis/src/redis-benchmark
PORT=7001
ulimit -n 200000

kill_all() { pkill -9 -f "/root/(kevy|srcbench)" 2>/dev/null; sleep 2; }

wait_ready() {
  for _ in $(seq 1 30); do
    "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_kevy_t() {
  kill_all
  local t=$1
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "0-$((t-1))" "$KBIN" --threads $t --port $PORT --no-aof < /dev/null >/tmp/k.log 2>&1 &
  disown; sleep 1; wait_ready
}
start_valkey() {
  kill_all
  setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 --protected-mode no --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes < /dev/null >/tmp/v.log 2>&1 &
  disown; sleep 2; wait_ready
}

# Args: op, c, n; runs 3 times and returns the median.
run_op_median() {
  local op=$1 c=$2 n=$3
  local r1 r2 r3
  r1=$(timeout 90 taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT -t "${op,,}" -c $c -P 1 -n $n -q 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
  r2=$(timeout 90 taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT -t "${op,,}" -c $c -P 1 -n $n -q 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
  r3=$(timeout 90 taskset -c 10-13 "$RB" -h 127.0.0.1 -p $PORT -t "${op,,}" -c $c -P 1 -n $n -q 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
  echo "${r1:-NA} ${r2:-NA} ${r3:-NA}" | tr ' ' '\n' | grep -v NA | sort -n | awk 'NR==2'
}

echo "=== K4 (ready-set bitmap arm-loop) bench ==="
echo "host=lx64; client cores 10-13; per-bench timeout 90s; n=1M; 3-run median"
echo ""
echo "=== K cliff (c=8000, c=10000) ==="
for c in 8000 10000; do
  echo "--- c=$c ---"
  for t in 1 2; do
    start_kevy_t $t || { echo "  kevy t=$t FAIL"; continue; }
    s=$(run_op_median SET $c 1000000)
    g=$(run_op_median GET $c 1000000)
    printf "  kevy t=%d  SET=%-12s  GET=%-12s\n" "$t" "$s" "$g"
  done
  start_valkey || { echo "  valkey FAIL"; continue; }
  s=$(run_op_median SET $c 1000000)
  g=$(run_op_median GET $c 1000000)
  printf "  valkey    SET=%-12s  GET=%-12s\n" "$s" "$g"
done

echo ""
echo "=== sanity matrix (must not regress) ==="
start_kevy_t 1 || exit 1
for spec in "c1-P1   SET 1   1  300000" \
            "c1-P1   GET 1   1  300000" \
            "c50-P1  SET 50  1  1000000" \
            "c50-P1  GET 50  1  1000000" \
            "c50-P16 SET 50  16 1000000" \
            "c50-P16 GET 50  16 1000000"; do
  read label op c p n <<<"$spec"
  v=$(timeout 60 taskset -c 10 "$RB" -h 127.0.0.1 -p $PORT -t "${op,,}" -c $c -P $p -n $n -q 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
  printf "  kevy %-9s %s = %s\n" "$label" "$op" "${v:-TIMEOUT}"
done
kill_all
echo "=== done ==="
