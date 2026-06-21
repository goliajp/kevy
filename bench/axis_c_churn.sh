#!/bin/bash
# Axis C: high key churn — -r {100k, 1M, 10M} random key namespace,
# SET-heavy. Each SET allocates a fresh key entry.
# Hypothesis: SmallBytes inline (≤22 B) means SET stores key + value
# in-bucket with 0 malloc per SET; valkey allocates a `robj` per
# SET regardless of size.
# Predict: kevy ≥120 % vs valkey on churn-heavy SET.
set -u
KBIN=${KBIN:-/root/kevy/target/release/kevy}
REDIS_BIN=/root/srcbench/redis/src/redis-server
VALKEY_BIN=/root/srcbench/valkey/src/valkey-server
REDIS_BENCH=/root/srcbench/redis/src/redis-benchmark
PORT=7001
RUNS=${RUNS:-3}
KEVY_THREADS=${KEVY_THREADS:-2}
KEVY_CORES=${KEVY_CORES:-0-1}
CMP_CORES=${CMP_CORES:-0-9}
CLI=${CLI:-10-13}

OUT=/tmp/axis_c.tsv
: > "$OUT"

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
  kill_all
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 \
    taskset -c "$KEVY_CORES" "$KBIN" --threads "$KEVY_THREADS" --port "$PORT" --no-aof \
    < /dev/null > /tmp/kevys.log 2>&1 &
  disown
  wait_ready || { echo "kevys not ready"; exit 1; }
}
start_redis() {
  kill_all
  setsid taskset -c "$CMP_CORES" "$REDIS_BIN" \
    --port "$PORT" --bind 127.0.0.1 --protected-mode no \
    --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes \
    --maxmemory 8gb \
    < /dev/null > /tmp/redis.log 2>&1 &
  disown
  wait_ready || { echo "redis not ready"; exit 1; }
}
start_valkey() {
  kill_all
  setsid taskset -c "$CMP_CORES" "$VALKEY_BIN" \
    --port "$PORT" --bind 127.0.0.1 --protected-mode no \
    --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes \
    --maxmemory 8gb \
    < /dev/null > /tmp/valkey.log 2>&1 &
  disown
  wait_ready || { echo "valkey not ready"; exit 1; }
}

bench_one() {
  local op_uc=$1 op_lc=$2 R=$3 N=$4
  local -a samples=()
  for _ in $(seq 1 "$RUNS"); do
    local out
    out=$(taskset -c "$CLI" "$REDIS_BENCH" -h 127.0.0.1 -p "$PORT" \
      -t "$op_lc" -n "$N" -c 50 -P 1 -r "$R" -q 2>&1 | tr '\r' '\n')
    local v
    v=$(echo "$out" | grep -E "^${op_uc}: [0-9]" | tail -1 | awk '{print $2}')
    samples+=("${v:-0}")
  done
  printf '%s\n' "${samples[@]}" | sort -n | awk -v n=${#samples[@]} 'NR==int(n/2)+1{print; exit}'
}

declare -a SCENS=(
  "100000 500000"
  "1000000 1000000"
  "10000000 2000000"
)

run_server() {
  local srv=$1 starter=$2
  echo "=== $srv ===" >&2
  $starter
  taskset -c 10 "$REDIS_BENCH" -h 127.0.0.1 -p "$PORT" -t set -n 50000 -q >/dev/null 2>&1
  for s in "${SCENS[@]}"; do
    read -r R N <<<"$s"
    # SET only — that's the churn workload.
    local rps; rps=$(bench_one "SET" "set" "$R" "$N")
    printf "%s\t%s\t%d\t%s\t%s\n" "churn" "$srv" "$R" "SET" "$rps" \
      | tee -a "$OUT"
  done
}

run_server kevys start_kevys
run_server redis start_redis
run_server valkey start_valkey
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
    _, srv, R, op, rps = parts
    if srv not in servers: servers.append(srv)
    data[(int(R), op)][srv] = float(rps)
print(f"| -r (keyspace) | op | {' | '.join(servers)} | kevy% best | verdict |")
print("|---|---|" + "|".join("---" for _ in servers) + "|---|---|")
for (R, op), row in sorted(data.items()):
    cells = [f"{row.get(s, 0):.0f}" for s in servers]
    best = max([row.get(s, 0) for s in servers if s != "kevys"] + [0])
    k = row.get("kevys", 0)
    pct = (k / best * 100) if best else 0
    verdict = "✅ ≥120%" if pct >= 120 else ("⚠ win<120%" if pct > 100 else ("tie" if 99 <= pct <= 101 else "❌ LOSS"))
    print(f"| {R} | {op} | {' | '.join(cells)} | {pct:.0f}% | {verdict} |")
PY
