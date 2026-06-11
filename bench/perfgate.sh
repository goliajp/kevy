#!/bin/bash
# perfgate — the throughput regression gate. No feature finishes and no
# release ships while this is red.
#
#   bash bench/perfgate.sh <KEVY_BIN>                    # gate against baseline
#   bash bench/perfgate.sh <KEVY_BIN> --update-baseline  # record a new baseline
#
# Runs on the dedicated bench box (lx64-class: io_uring, >=16 hw threads,
# cores 0-7 for the server / 8-15 for the load generators). Angles and
# discipline encode every measurement trap the 2026-06-10 campaign hit:
#
#   * pinned-hashtag angle, ONE TEST PER INVOCATION, long N — plain-mode
#     redis-benchmark pinned per shard via {tag}. `--cluster` client mode is
#     client-bound (~6.6M) and skews keys across nodes when -t lists several
#     tests; short N under-amortises ramp by ~30%. Never measure with those.
#   * legacy 8sh fixed-key angle — historical comparability (REPORT.md).
#   * preflight refuses to run on a dirty box (leftover kevy/redis-benchmark
#     processes, or load >= 1.0): a polluted run costs hours of false
#     debugging, a refused run costs one retry.
#   * 3 fresh server INSTANCES per angle, median across instances, compared
#     against bench/PERF-BASELINE.json with per-metric tolerance. Instance-
#     to-instance spread (page placement / IRQ luck at server start) proved
#     the dominant noise axis (2026-06-11: +-5% on legacy_8sh_get — three
#     rounds against ONE instance just re-sample the same draw, and a gate
#     conditioned on a single instance is a coin flip near the floor).
#     Rounds within an instance swing only +-2%.
#
# Exit codes: 0 = PASS (or baseline updated), 1 = FAIL/regression, 2 = refused
# (dirty box / missing tools / bad usage).
set -u

BIN=${1:?usage: perfgate.sh <KEVY_BIN> [--update-baseline]}
MODE=${2:-gate}
HERE=$(cd "$(dirname "$0")" && pwd)
BASELINE="$HERE/PERF-BASELINE.json"
N_PINNED=${N_PINNED:-30000000}   # per process x8 — long N, ramp amortised
N_LEGACY=${N_LEGACY:-30000000}
INSTANCES=${INSTANCES:-3}
# Hashtags pinning shard 0..7 under the contiguous slot split (CRC16).
TAGS=(t3 t43 t2 t42 t1 t41 t0 t40)

refuse() { echo "perfgate: REFUSED — $1" >&2; exit 2; }
fail()   { echo "perfgate: FAIL — $1" >&2; exit 1; }

# ---------- preflight: never measure on a dirty box ----------
command -v redis-benchmark >/dev/null || refuse "redis-benchmark not installed"
[ -x "$BIN" ] || refuse "$BIN is not executable"
LEFTOVER=$(pgrep -af "kevy|redis-benchmark" | grep -v perfgate | grep -v claude || true)
[ -n "$LEFTOVER" ] && refuse "leftover bench processes (sweep first):
$LEFTOVER"
# Instantaneous idle%, not 1-min loadavg: loadavg measures the past, so a
# back-to-back run (baseline then gate) would refuse on its own wake. Two
# /proc/stat samples 1s apart = what the box is doing RIGHT NOW.
read -r _ u1 n1 s1 i1 _ < /proc/stat; sleep 1; read -r _ u2 n2 s2 i2 _ < /proc/stat
IDLE=$(( (i2 - i1) * 100 / ( (u2-u1) + (n2-n1) + (s2-s1) + (i2-i1) ) ))
[ "$IDLE" -ge 80 ] || refuse "box busy (idle ${IDLE}% < 80%)"

server_stop() {
  pkill -f "^$BIN" 2>/dev/null
  while pgrep -f "^$BIN" >/dev/null; do sleep 0.1; done
}
server_start() { # $1 = extra flags
  server_stop
  env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0-7 \
    "$BIN" --threads 8 --port 7001 $1 --no-aof >/tmp/perfgate_srv.log 2>&1 &
  SRV=$!
  for _ in $(seq 1 100); do
    timeout 2 redis-benchmark -p 7001 -t ping -n 1 -c 1 -q >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  refuse "server did not come up (see /tmp/perfgate_srv.log)"
}

sum_rps() { # files...
  cat "$@" | tr "\r" "\n" | grep -oE "[0-9.]+ requests per second" \
    | awk '{s+=$1} END {printf "%.0f", s}'
}

run_pinned() { # $1 = get|set, $2 = cluster|compat -> echoes total rps
  local t=$1 mode=$2 pids=() outs=() port i tag out
  for i in $(seq 0 7); do
    port=7001; [ "$mode" = cluster ] && port=$((7002 + i))
    tag=${TAGS[$i]}
    out=/tmp/perfgate_${mode}_${t}_$i.out; outs+=("$out")
    if [ "$t" = set ]; then
      taskset -c 8-15 redis-benchmark -p $port -n "$N_PINNED" -r 1000000 \
        -c 6 -P 256 -q SET "{$tag}:__rand_int__" v >"$out" 2>&1 &
    else
      taskset -c 8-15 redis-benchmark -p $port -n "$N_PINNED" -r 1000000 \
        -c 6 -P 256 -q GET "{$tag}:__rand_int__" >"$out" 2>&1 &
    fi
    pids+=($!)
  done
  wait "${pids[@]}"
  sum_rps "${outs[@]}"
}

run_legacy() { # $1 = get|set -> echoes overall rps (fixed key, REUSEPORT)
  local t=$1 raw
  raw=$(taskset -c 8-15 redis-benchmark -h 127.0.0.1 -p 7001 -t "$t" \
    -n "$N_LEGACY" -c 50 -P 256 --threads 8 -q 2>/dev/null)
  printf "%s" "$raw" | tr "\r" "\n" \
    | grep -oE "[0-9.]+ requests per second" | tail -1 \
    | awk '{printf "%.0f", $1}'
}

median_of() { printf "%s\n" "$@" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

# ---------- measure: 3 fresh instances per angle, median across them ----------
# Each instance pass boots a brand-new server (cluster then legacy), warms
# it, and measures every angle once — so each metric collects one sample
# per instance and the medians compare instance-distribution centers, not
# one instance's luck.
declare -A SAMPLES MED
sample() { # $1 metric-name, $2 command-string
  local v
  v=$(eval "$2")
  SAMPLES[$1]="${SAMPLES[$1]:-}$v "
}

echo "perfgate: warming + measuring (bin=$BIN, N=$N_PINNED x8, $INSTANCES instances/angle)"
for inst in $(seq 1 "$INSTANCES"); do
  server_start "--cluster"
  # Warm each shard's keyspace (1M random keys per tag at P64).
  # NB: wait MUST name the pids — a bare `wait` also waits on the $SRV
  # background job, which never exits (this exact hang has bitten twice).
  WPIDS=()
  for i in $(seq 0 7); do
    taskset -c 8-15 redis-benchmark -p $((7002 + i)) -n 1000000 -r 1000000 \
      -P 64 -q SET "{${TAGS[$i]}}:__rand_int__" v >/dev/null 2>&1 &
    WPIDS+=($!)
  done; wait "${WPIDS[@]}"
  sample pinned_cluster_get 'run_pinned get cluster'
  sample pinned_cluster_set 'run_pinned set cluster'
  sample pinned_compat_get  'run_pinned get compat'
  sample pinned_compat_set  'run_pinned set compat'
  server_start ""   # legacy angle: cluster off (the historical configuration)
  taskset -c 8-15 redis-benchmark -p 7001 -t set -n 300000 -P 64 -q >/dev/null 2>&1
  sample legacy_8sh_get 'run_legacy get'
  sample legacy_8sh_set 'run_legacy set'
  server_stop
done
for m in pinned_cluster_get pinned_cluster_set pinned_compat_get pinned_compat_set \
         legacy_8sh_get legacy_8sh_set; do
  # shellcheck disable=SC2086 — word-splitting the collected samples is the point
  MED[$m]=$(median_of ${SAMPLES[$m]})
  echo "  $m: instances [${SAMPLES[$m]%% }] -> median ${MED[$m]}"
done

# ---------- compare or record ----------
if [ "$MODE" = "--update-baseline" ]; then
  {
    echo "{"
    echo "  \"recorded\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\","
    echo "  \"bin\": \"$(basename "$BIN")\","
    echo "  \"tolerance\": 0.92,"
    echo "  \"metrics\": {"
    first=1
    for k in pinned_cluster_get pinned_cluster_set pinned_compat_get \
             pinned_compat_set legacy_8sh_get legacy_8sh_set; do
      [ $first -eq 0 ] && echo ","
      printf '    "%s": %s' "$k" "${MED[$k]}"
      first=0
    done
    echo ""
    echo "  }"
    echo "}"
  } > "$BASELINE"
  echo "perfgate: baseline recorded -> $BASELINE"
  exit 0
fi

[ -f "$BASELINE" ] || refuse "no baseline ($BASELINE) — run with --update-baseline first"
TOL=$(python3 -c "import json;print(json.load(open('$BASELINE'))['tolerance'])")
STATUS=0
echo "perfgate: gate vs $(basename "$BASELINE") (floor = baseline x $TOL)"
for k in pinned_cluster_get pinned_cluster_set pinned_compat_get \
         pinned_compat_set legacy_8sh_get legacy_8sh_set; do
  BASE=$(python3 -c "import json;print(json.load(open('$BASELINE'))['metrics']['$k'])")
  FLOOR=$(awk -v b="$BASE" -v t="$TOL" 'BEGIN{printf "%.0f", b*t}')
  GOT=${MED[$k]}
  if [ "$GOT" -lt "$FLOOR" ]; then
    echo "  ✗ $k: $GOT < floor $FLOOR (baseline $BASE)"; STATUS=1
  else
    echo "  ✓ $k: $GOT >= floor $FLOOR (baseline $BASE)"
  fi
done
[ $STATUS -eq 0 ] && echo "perfgate: PASS" || echo "perfgate: FAIL — regression; do not finish/release" >&2
exit $STATUS
