#!/bin/bash
# 4-way matrix bench: kevy / valkey 9.1.0 / redis 8.8.0
#
# Each scenario runs 3 times, prints all + the median (sort -n | head -2 | tail -1).
# All TCP servers built from source on lx64, default jemalloc, no TLS, no
# persistence. Server pinned cores 0-9; client core 10 (multi: 10-13).
#
# Output: bench/matrix-results.md
set -u

SRV_CORES=${SRV_CORES:-0-9}
CLI_CORE=${CLI_CORE:-10}
CLI_CORES_MULTI=${CLI_CORES_MULTI:-10-13}
KEVY_BIN=${KEVY_BIN:-/root/kevy/target/release-perf/kevy}
REDIS_BIN=/root/srcbench/redis/src/redis-server
VALKEY_BIN=/root/srcbench/valkey/src/valkey-server
REDIS_BENCH=/root/srcbench/redis/src/redis-benchmark
PORT=7001
N_C1=${N_C1:-300000}
N_CN=${N_CN:-1000000}
WARM_N=50000
OUT=${OUT:-/tmp/matrix-results.md}
RUNS=${RUNS:-3}

die() { echo "FATAL: $*" >&2; exit 1; }
for f in "$KEVY_BIN" "$REDIS_BIN" "$VALKEY_BIN" "$REDIS_BENCH"; do
  [ -x "$f" ] || die "missing binary: $f"
done

kill_servers() {
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
  kill_servers
  nohup env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 \
    taskset -c "$SRV_CORES" "$KEVY_BIN" --threads 10 --port "$PORT" --no-aof \
    > /tmp/kevys.log 2>&1 &
  wait_ready || die "kevys not ready"
}
start_redis() {
  kill_servers
  nohup taskset -c "$SRV_CORES" "$REDIS_BIN" \
    --port "$PORT" --bind 127.0.0.1 --protected-mode no \
    --save '' --appendonly no \
    --io-threads 10 --io-threads-do-reads yes \
    > /tmp/redis.log 2>&1 &
  wait_ready || die "redis not ready"
}
start_valkey() {
  kill_servers
  nohup taskset -c "$SRV_CORES" "$VALKEY_BIN" \
    --port "$PORT" --bind 127.0.0.1 --protected-mode no \
    --save '' --appendonly no \
    --io-threads 10 --io-threads-do-reads yes \
    > /tmp/valkey.log 2>&1 &
  wait_ready || die "valkey not ready"
}

# Median of N. Args: number of values then values.
median() {
  printf '%s\n' "$@" | sort -n | awk -v n=$# 'NR==int(n/2)+1{print; exit}'
}

# Run a single redis-benchmark, echo full stdout in a single line via tr.
bench_raw() {
  taskset -c "$1" "$REDIS_BENCH" -h 127.0.0.1 -p "$PORT" "${@:2}" 2>&1 | tr '\r' '\n'
}

# Extract rps for given uppercase op (e.g. SET) from one bench_raw output.
extract_rps() {
  local op=$1 file=$2
  grep -E "^${op}: [0-9]" "$file" | tail -1 | awk '{print $2}'
}

# Run scenario RUNS times, median per op. Args: label, ops_uc_csv, cores, srv,
# rest of redis-benchmark args (excluding -t).
run_scenario() {
  local label=$1 ops_uc=$2 cores=$3 srv=$4
  shift 4
  # redis-benchmark accepts uppercase op names directly via -t.
  local ops_lc
  ops_lc=$(printf '%s' "$ops_uc" | tr 'A-Z' 'a-z')
  local -A medians=()
  local -A samples_str=()
  for op in ${ops_uc//,/ }; do medians[$op]=""; samples_str[$op]=""; done
  local tmpf
  tmpf=$(mktemp)
  for _ in $(seq 1 "$RUNS"); do
    bench_raw "$cores" -t "$ops_lc" "$@" > "$tmpf"
    for op in ${ops_uc//,/ }; do
      local v
      v=$(extract_rps "$op" "$tmpf")
      samples_str[$op]="${samples_str[$op]} ${v:-0}"
    done
  done
  rm -f "$tmpf"
  for op in ${ops_uc//,/ }; do
    medians[$op]=$(median ${samples_str[$op]})
  done
  local row="$label	$srv"
  for op in ${ops_uc//,/ }; do
    row="$row	${medians[$op]}"
  done
  printf '%s\n' "$row"
}

# ---- main runner ----

run_all() {
  local srv=$1
  # Warm-up
  taskset -c "$CLI_CORE" "$REDIS_BENCH" -h 127.0.0.1 -p "$PORT" \
    -t set -n "$WARM_N" -q >/dev/null 2>&1
  run_scenario "c1-P1" "SET,GET" "$CLI_CORE" "$srv" -n "$N_C1" -c 1 -P 1 -q
  run_scenario "c50-P1" "SET,GET" "$CLI_CORES_MULTI" "$srv" -n "$N_CN" -c 50 -P 1 -q
  run_scenario "c50-P16" "SET,GET" "$CLI_CORES_MULTI" "$srv" -n "$N_CN" -c 50 -P 16 -q
  run_scenario "c100-P1" "SET,GET" "$CLI_CORES_MULTI" "$srv" -n "$N_CN" -c 100 -P 1 -q
  run_scenario "c50-INCR" "INCR" "$CLI_CORES_MULTI" "$srv" -n "$N_CN" -c 50 -P 1 -q
  run_scenario "c50-MSET" "MSET" "$CLI_CORES_MULTI" "$srv" -n "$N_CN" -c 50 -P 1 -q
  run_scenario "c50-10KB" "SET,GET" "$CLI_CORES_MULTI" "$srv" -n 200000 -c 50 -P 1 -d 10240 -q
}

mkdir -p "$(dirname "$OUT")"
echo "# kevy / valkey / redis matrix bench (median of $RUNS runs) — $(date -u +%Y-%m-%dT%H:%MZ)" > "$OUT"
echo "" >> "$OUT"
echo "Host lx64 (Intel i7-10700K Comet Lake, 16 cores, mitigations=off)." >> "$OUT"
echo "" >> "$OUT"
echo '```' >> "$OUT"

TMP=$(mktemp)
trap "rm -f $TMP" EXIT
printf "scenario\tserver\top1\top2\n" | tee -a "$OUT"

for cfg in "kevys:start_kevys" "redis:start_redis" "valkey:start_valkey"; do
  srv="${cfg%:*}"
  starter="${cfg#*:}"
  echo "## $srv ##" >&2
  $starter
  run_all "$srv" | tee -a "$TMP" | tee -a "$OUT"
done

kill_servers
echo '```' >> "$OUT"

# Pivot.
{
  echo ""
  echo "## Pivoted (RPS, kevy% of best-competitor)"
  echo ""
  python3 - "$TMP" <<'PY'
import sys
from collections import OrderedDict

rows = OrderedDict()
servers = []
for line in open(sys.argv[1]):
    line = line.rstrip('\n')
    if not line:
        continue
    parts = line.split('\t')
    if len(parts) < 4:
        continue
    scn, srv = parts[0], parts[1]
    vals = parts[2:]
    rows.setdefault(scn, {})[srv] = vals
    if srv not in servers:
        servers.append(srv)

# Build header
def fnum(s):
    try:
        return float(s)
    except (ValueError, TypeError):
        return None

ops_for = {
    "c1-P1": ("SET", "GET"),
    "c50-P1": ("SET", "GET"),
    "c50-P16": ("SET", "GET"),
    "c100-P1": ("SET", "GET"),
    "c50-INCR": ("INCR",),
    "c50-MSET": ("MSET",),
    "c50-10KB": ("SET", "GET"),
}

header_ops = []
for scn, vals_per_srv in rows.items():
    for op in ops_for.get(scn, ("op1", "op2"))[:max(len(v) for v in vals_per_srv.values())]:
        pass

print("| scenario | op | kevys | valkey | redis | kevy / best-competitor |")
print("|---|---|---|---|---|---|")
for scn, by_srv in rows.items():
    op_names = ops_for.get(scn, ("op1", "op2"))
    n_ops = max(len(v) for v in by_srv.values())
    for i in range(n_ops):
        op = op_names[i] if i < len(op_names) else f"op{i+1}"
        k = by_srv.get("kevys", [])
        v = by_srv.get("valkey", [])
        r = by_srv.get("redis", [])
        k_v = fnum(k[i]) if i < len(k) else None
        v_v = fnum(v[i]) if i < len(v) else None
        r_v = fnum(r[i]) if i < len(r) else None
        comp = max([x for x in [v_v, r_v] if x is not None], default=None)
        pct = f"{(k_v/comp)*100:.0f}%" if k_v and comp else "-"
        verdict = ""
        if k_v and comp:
            if k_v / comp >= 1.20:
                verdict = " ✅ ≥120%"
            elif k_v / comp >= 1.00:
                verdict = " ⚠ win<120%"
            else:
                verdict = " ❌ LOSS"
        ks = f"{k_v:.0f}" if k_v else "-"
        vs = f"{v_v:.0f}" if v_v else "-"
        rs = f"{r_v:.0f}" if r_v else "-"
        print(f"| {scn} | {op} | {ks} | {vs} | {rs} | {pct}{verdict} |")
PY
} >> "$OUT"

echo
echo "wrote $OUT"
