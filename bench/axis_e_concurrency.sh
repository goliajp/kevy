#!/bin/bash
# Axis E: deep concurrency sweep — -c 50 / 200 / 500 / 1000 / 2000.
# Hypothesis: shared-nothing thread-per-core + kevy-ring SPSC scales
# linearly with conn count; valkey's single-dispatcher (main thread
# does all command dispatch) saturates beyond ~1k conns.
# Predict: kevy ≥120 % of valkey at -c 500+.
set -u
KBIN=${KBIN:-/root/kevy/target/release/kevy}
REDIS_BIN=/root/srcbench/redis/src/redis-server
VALKEY_BIN=/root/srcbench/valkey/src/valkey-server
REDIS_BENCH=/root/srcbench/redis/src/redis-benchmark
PORT=7001
RUNS=${RUNS:-3}
KEVY_THREADS_LOW=${KEVY_THREADS_LOW:-2}    # for -c <= 100
KEVY_THREADS_HIGH=${KEVY_THREADS_HIGH:-10} # for -c >= 500
CMP_CORES=${CMP_CORES:-0-9}
CLI=${CLI:-10-13}

OUT=/tmp/axis_e.tsv
: > "$OUT"

ulimit -n 65536 2>/dev/null

kill_all() {
  pkill -9 -f /root/kevy/target 2>/dev/null
  pkill -9 -f /root/srcbench/redis 2>/dev/null
  pkill -9 -f /root/srcbench/valkey 2>/dev/null
  sleep 1
}
wait_ready() {
  for _ in $(seq 1 50); do
    "$REDIS_BENCH" -h 127.0.0.1 -p "$PORT" -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}
start_kevys() {
  local T=$1
  local CORES="0-$((T-1))"
  kill_all
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 \
    taskset -c "$CORES" "$KBIN" --threads "$T" --port "$PORT" --no-aof \
    < /dev/null > /tmp/kevys.log 2>&1 &
  disown
  wait_ready || { echo "kevys not ready (T=$T)"; exit 1; }
}
start_redis() {
  kill_all
  setsid taskset -c "$CMP_CORES" "$REDIS_BIN" \
    --port "$PORT" --bind 127.0.0.1 --protected-mode no \
    --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes \
    --maxclients 65535 \
    < /dev/null > /tmp/redis.log 2>&1 &
  disown
  wait_ready || { echo "redis not ready"; exit 1; }
}
start_valkey() {
  kill_all
  setsid taskset -c "$CMP_CORES" "$VALKEY_BIN" \
    --port "$PORT" --bind 127.0.0.1 --protected-mode no \
    --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes \
    --maxclients 65535 \
    < /dev/null > /tmp/valkey.log 2>&1 &
  disown
  wait_ready || { echo "valkey not ready"; exit 1; }
}

bench_one() {
  local op_uc=$1 op_lc=$2 C=$3 N=$4
  local -a samples=()
  for _ in $(seq 1 "$RUNS"); do
    local out
    out=$(taskset -c "$CLI" "$REDIS_BENCH" -h 127.0.0.1 -p "$PORT" \
      -t "$op_lc" -n "$N" -c "$C" -P 1 -q 2>&1 | tr '\r' '\n')
    local v
    v=$(echo "$out" | grep -E "^${op_uc}: [0-9]" | tail -1 | awk '{print $2}')
    samples+=("${v:-0}")
  done
  printf '%s\n' "${samples[@]}" | sort -n | awk -v n=${#samples[@]} 'NR==int(n/2)+1{print; exit}'
}

# (C, N) pairs.
declare -a SCENS=(
  "50  1000000"
  "200 1000000"
  "500 1000000"
  "1000 1000000"
  "2000 1000000"
)

run_kevys_axis() {
  echo "=== kevys ===" >&2
  for s in "${SCENS[@]}"; do
    read -r C N <<<"$s"
    # Use more threads for high-conn cases.
    if [ "$C" -ge 500 ]; then T=$KEVY_THREADS_HIGH; else T=$KEVY_THREADS_LOW; fi
    start_kevys "$T"
    taskset -c 10 "$REDIS_BENCH" -h 127.0.0.1 -p "$PORT" -t set -n 50000 -q >/dev/null 2>&1
    for op in SET GET; do
      local lc; lc=$(echo "$op" | tr 'A-Z' 'a-z')
      local rps; rps=$(bench_one "$op" "$lc" "$C" "$N")
      printf "%s\t%s\t%d\t%s\t%s\n" "concurrency" "kevys" "$C" "$op" "$rps" \
        | tee -a "$OUT"
    done
  done
}

run_cmp_axis() {
  local srv=$1 starter=$2
  echo "=== $srv ===" >&2
  $starter
  taskset -c 10 "$REDIS_BENCH" -h 127.0.0.1 -p "$PORT" -t set -n 50000 -q >/dev/null 2>&1
  for s in "${SCENS[@]}"; do
    read -r C N <<<"$s"
    for op in SET GET; do
      local lc; lc=$(echo "$op" | tr 'A-Z' 'a-z')
      local rps; rps=$(bench_one "$op" "$lc" "$C" "$N")
      printf "%s\t%s\t%d\t%s\t%s\n" "concurrency" "$srv" "$C" "$op" "$rps" \
        | tee -a "$OUT"
    done
  done
}

run_kevys_axis
run_cmp_axis redis start_redis
run_cmp_axis valkey start_valkey
kill_all

echo "=== pivoted ==="
python3 - "$OUT" <<'PY'
import sys
from collections import defaultdict
data = defaultdict(dict)
servers = []
for ln in open(sys.argv[1]):
    parts = ln.strip().split('\t')
    if len(parts) != 5: continue
    _, srv, C, op, rps = parts
    if srv not in servers: servers.append(srv)
    data[(int(C), op)][srv] = float(rps)
print(f"| -c | op | {' | '.join(servers)} | kevy% best | verdict |")
print("|---|---|" + "|".join("---" for _ in servers) + "|---|---|")
for (C, op), row in sorted(data.items()):
    cells = [f"{row.get(s, 0):.0f}" for s in servers]
    best = max([row.get(s, 0) for s in servers if s != "kevys"] + [0])
    k = row.get("kevys", 0)
    pct = (k / best * 100) if best else 0
    verdict = "✅ ≥120%" if pct >= 120 else ("⚠ win<120%" if pct > 100 else ("tie" if 99 <= pct <= 101 else "❌ LOSS"))
    print(f"| {C} | {op} | {' | '.join(cells)} | {pct:.0f}% | {verdict} |")
PY
