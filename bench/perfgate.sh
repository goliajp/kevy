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
#     Since v4 T4 the same topology also gates the five arena cells
#     (INCR/SADD/HSET/LPUSH/ZADD) so the observation items ratchet
#     automatically instead of living as per-release ledger footnotes.
#   * preflight refuses to run on a dirty box (leftover kevy/redis-benchmark
#     processes, or load >= 1.0): a polluted run costs hours of false
#     debugging, a refused run costs one retry.
#   * 3 fresh server INSTANCES per angle, median across instances, compared
#     against bench/PERF-BASELINE.json with per-metric tolerance.
#   * the --threads angles do NOT read redis-benchmark's reported rps.
#     Under --threads, the benchmark's only exit is its own 250ms
#     showThroughput timer (redis-benchmark.c:52 SHOW_THROUGHPUT_INTERVAL,
#     :1653 aeStop; without --threads it stops in clientDone at :425), so
#     totlatency is rounded UP to a multiple of 250ms and the reported
#     throughput is quantized to N/(k*250ms) — 7% buckets at this angle —
#     and systematically understated. Every "instance mode" and "attractor"
#     this gate ever chased was a bucket of that grid; the 2026-06-11 note
#     calling instance spread "the dominant noise axis" was reading the
#     ruler, not the box. These angles now take throughput from the server's
#     own command counter over a wall window timed here, which has no grid.
#     See bench/PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md.
#
# Exit codes: 0 = PASS (or baseline updated), 1 = FAIL/regression, 2 = refused
# (dirty box / missing tools / bad usage).
set -u

BIN=${1:?usage: perfgate.sh <KEVY_BIN> [--update-baseline]}
MODE=${2:-gate}
HERE=$(cd "$(dirname "$0")" && pwd)
BASELINE="$HERE/PERF-BASELINE.json"
N_PINNED=${N_PINNED:-30000000}   # per process x8 — long N, ramp amortised
# The --threads angles measure a wall window, not a request count, so N only
# has to outlast RAMP + WINDOW at the slowest angle. 60M at ~3M ops/s is 20s
# of headroom over a 4s measurement.
N_LEGACY=${N_LEGACY:-60000000}
RAMP=${RAMP:-1.0}                # skip the connect/ramp transient
WINDOW=${WINDOW:-3.0}            # the measured steady-state window
INSTANCES=${INSTANCES:-3}
# Hashtags pinning shard 0..7 under the contiguous slot split (CRC16).
TAGS=(t3 t43 t2 t42 t1 t41 t0 t40)

refuse() { echo "perfgate: REFUSED — $1" >&2; exit 2; }
fail()   { echo "perfgate: FAIL — $1" >&2; exit 1; }

# Every gated metric, in report order. The single source of truth for
# the measure loop, --update-baseline, and the gate comparison — three
# hand-copied lists drifted once already.
METRICS="pinned_cluster_get pinned_cluster_set pinned_compat_get pinned_compat_set \
legacy_8sh_get legacy_8sh_set \
legacy_8sh_incr legacy_8sh_sadd legacy_8sh_hset legacy_8sh_lpush legacy_8sh_zadd \
zalg_zinterstore"

# ---------- preflight: never measure on a dirty box ----------
command -v redis-benchmark >/dev/null || refuse "redis-benchmark not installed"
command -v redis-cli >/dev/null || refuse "redis-cli not installed (the --threads angles read the server's counter through it)"
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

# The server's own view of how much work it did. `--threads` makes
# redis-benchmark's reported rps a quantized, understated number (see the
# header); the command counter does not lie about that. Two INFO calls per
# measurement are lost in the noise of tens of millions of ops.
srv_cmds() {
  redis-cli -p 7001 INFO stats 2>/dev/null | tr -d '\r' \
    | awk -F: '/^total_commands_processed:/ {print $2}'
}

# Drive the load, then read the server counter across a window we time
# ourselves. The load generator is left running for the whole window and
# killed afterwards, so N only has to be large enough to outlast RAMP +
# WINDOW — it is not the unit of measurement any more.
steady_rps() { # $1... = the redis-benchmark argv after the pinning
  local bpid c0 t0 c1 t1
  taskset -c 8-15 "$@" >/dev/null 2>&1 &
  bpid=$!
  sleep "$RAMP"
  c0=$(srv_cmds); t0=$(date +%s%N)
  sleep "$WINDOW"
  c1=$(srv_cmds); t1=$(date +%s%N)
  kill "$bpid" 2>/dev/null
  wait "$bpid" 2>/dev/null
  [ -n "$c0" ] && [ -n "$c1" ] || refuse "server counter unreadable (INFO stats)"
  awk -v c0="$c0" -v c1="$c1" -v t0="$t0" -v t1="$t1" \
    'BEGIN {printf "%.0f", (c1 - c0) / ((t1 - t0) / 1e9)}'
}

run_legacy() { # $1 = get|set|incr|... -> steady-state ops/s (fixed key, REUSEPORT)
  steady_rps redis-benchmark -h 127.0.0.1 -p 7001 -t "$1" \
    -n "$N_LEGACY" -c 50 -P 256 --threads 8 -q
}

# v2.2 zset-algebra hot line: pipelined ZINTERSTORE over two warmed
# zsets on the plain-mode server (arbitrary-command form; reuses the
# legacy topology). Sources warmed once per instance by the caller.
run_zalg() {
  steady_rps redis-benchmark -h 127.0.0.1 -p 7001 \
    -n "$N_LEGACY" -c 50 -P 16 --threads 8 -q \
    ZINTERSTORE "zalg:dst:__rand_int__" 2 zalg:a zalg:b
}

median_of() { printf "%s\n" "$@" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

# ---------- the reference binary ----------
# The gate is RELATIVE: the candidate is compared against a binary built from
# the baseline's own commit, measured on this box in this session. Comparing
# today's number against a number recorded weeks ago cannot work here — the
# box drifts (2026-07-12: the SAME code measured 24.3M when the baseline was
# taken and 21.0M three weeks later, a 13% slide that dwarfs the 8% gate).
# Interleaving the two binaries makes that drift cancel instead of deciding
# the verdict.
ref_binary() {
  local sha=$1 cache="$HERE/.perfgate-ref" out="$HERE/.perfgate-ref/kevy-${sha:0:12}"
  [ -x "$out" ] && { printf "%s" "$out"; return 0; }
  mkdir -p "$cache"
  local wt="$cache/wt-${sha:0:12}"
  git -C "$HERE/.." worktree add -f --detach "$wt" "$sha" >/dev/null 2>&1 \
    || refuse "cannot check out the baseline commit $sha (fetch it first?)"
  ( cd "$wt" && cargo build -q --profile release-perf -p kevy --bin kevy ) \
    || { git -C "$HERE/.." worktree remove --force "$wt" >/dev/null 2>&1; refuse "reference build failed at $sha"; }
  cp "$wt/target/release-perf/kevy" "$out"
  git -C "$HERE/.." worktree remove --force "$wt" >/dev/null 2>&1
  printf "%s" "$out"
}

# ---------- measure ----------
# One instance pass = boot a fresh server, warm it, measure every angle once.
# Each instance measures BOTH binaries, and the order flips every instance:
# the box slows monotonically across a long run, so a fixed "reference first,
# candidate second" order systematically favours whichever went first. That
# single confound is what manufactured the "disjoint distributions" of the
# v4 SET regression that never existed.
declare -A SAMPLES
sample() { # $1 = cand|ref, $2 = metric, $3 = value
  SAMPLES["$1:$2"]="${SAMPLES["$1:$2"]:-}$3 "
}

measure_all() { # $1 = cand|ref, $2 = binary
  local who=$1 i j args
  BIN=$2
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
  sample "$who" pinned_cluster_get "$(run_pinned get cluster)"
  sample "$who" pinned_cluster_set "$(run_pinned set cluster)"
  sample "$who" pinned_compat_get  "$(run_pinned get compat)"
  sample "$who" pinned_compat_set  "$(run_pinned set compat)"

  server_start ""   # legacy angle: cluster off (the historical configuration)
  taskset -c 8-15 redis-benchmark -p 7001 -t set -n 300000 -P 64 -q >/dev/null 2>&1
  sample "$who" legacy_8sh_get   "$(run_legacy get)"
  sample "$who" legacy_8sh_set   "$(run_legacy set)"
  # The five arena cells, gated here instead of living as per-release ledger
  # footnotes (LPUSH and ZADD are the historically noisiest of them).
  sample "$who" legacy_8sh_incr  "$(run_legacy incr)"
  sample "$who" legacy_8sh_sadd  "$(run_legacy sadd)"
  sample "$who" legacy_8sh_hset  "$(run_legacy hset)"
  sample "$who" legacy_8sh_lpush "$(run_legacy lpush)"
  sample "$who" legacy_8sh_zadd  "$(run_legacy zadd)"
  # zalgebra angle: two 1k-member source zsets, then pipelined ZINTERSTORE.
  for i in $(seq 0 9); do
    args=""
    for j in $(seq 0 99); do args="$args $((i*100+j)) m$((i*100+j))"; done
    redis-cli -p 7001 ZADD zalg:a $args >/dev/null 2>&1
    redis-cli -p 7001 ZADD zalg:b $args >/dev/null 2>&1
  done
  sample "$who" zalg_zinterstore "$(run_zalg)"
  server_stop
}

CAND_BIN=$BIN
if [ "$MODE" = "--update-baseline" ]; then
  echo "perfgate: recording a baseline from $(basename "$CAND_BIN") ($INSTANCES instances/angle)"
  for inst in $(seq 1 "$INSTANCES"); do measure_all cand "$CAND_BIN"; done
else
  [ -f "$BASELINE" ] || refuse "no baseline ($BASELINE) — run with --update-baseline first"
  REF_SHA=$(python3 -c "import json;print(json.load(open('$BASELINE')).get('ref_commit',''))")
  [ -n "$REF_SHA" ] || refuse "baseline has no ref_commit — re-record it with --update-baseline"
  REF_BIN=${PERFGATE_REF_BIN:-$(ref_binary "$REF_SHA")}
  echo "perfgate: candidate $(basename "$CAND_BIN") vs reference ${REF_SHA:0:12}, interleaved, $INSTANCES instances/angle"
  for inst in $(seq 1 "$INSTANCES"); do
    if [ $((inst % 2)) -eq 1 ]; then
      measure_all ref "$REF_BIN"; measure_all cand "$CAND_BIN"
    else
      measure_all cand "$CAND_BIN"; measure_all ref "$REF_BIN"
    fi
  done
fi

declare -A MED REF_MED
for m in $METRICS; do
  # shellcheck disable=SC2086 — word-splitting the collected samples is the point
  MED[$m]=$(median_of ${SAMPLES["cand:$m"]})
  if [ "$MODE" != "--update-baseline" ]; then
    # shellcheck disable=SC2086
    REF_MED[$m]=$(median_of ${SAMPLES["ref:$m"]})
  fi
done

# ---------- compare or record ----------
# The recorded numbers are the reference commit plus that commit's absolute
# medians. The medians are for the trend record only — the gate never reads
# them, because an absolute number recorded on a past box state is not a
# statement about the code. What the gate reads is `ref_commit`, which it
# rebuilds and re-measures alongside the candidate.
if [ "$MODE" = "--update-baseline" ]; then
  REF_SHA=$(git -C "$HERE/.." rev-parse HEAD)
  {
    echo "{"
    echo "  \"recorded\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\","
    echo "  \"bin\": \"$(basename "$CAND_BIN")\","
    echo "  \"ref_commit\": \"$REF_SHA\","
    echo "  \"tolerance\": 0.92,"
    echo "  \"_note\": \"metrics are the reference commit's medians on the box of the day — a trend record, not the gate. The gate rebuilds ref_commit and measures it against the candidate in the same session.\","
    echo "  \"metrics\": {"
    first=1
    for k in $METRICS; do
      [ $first -eq 0 ] && echo ","
      printf '    "%s": %s' "$k" "${MED[$k]}"
      first=0
    done
    echo ""
    echo "  }"
    echo "}"
  } > "$BASELINE"
  echo "perfgate: baseline recorded (ref_commit ${REF_SHA:0:12}) -> $BASELINE"
  exit 0
fi

TOL=$(python3 -c "import json;print(json.load(open('$BASELINE'))['tolerance'])")
STATUS=0
echo "perfgate: gate — candidate vs reference ${REF_SHA:0:12}, both measured just now (floor = reference x $TOL)"
for k in $METRICS; do
  GOT=${MED[$k]}
  REF=${REF_MED[$k]}
  if [ -z "$REF" ] || [ "$REF" -eq 0 ] 2>/dev/null; then
    echo "  ~ $k: $GOT (reference produced no sample — angle is new to the reference commit)"
    continue
  fi
  FLOOR=$(awk -v b="$REF" -v t="$TOL" 'BEGIN{printf "%.0f", b*t}')
  RATIO=$(awk -v g="$GOT" -v b="$REF" 'BEGIN{printf "%+.1f%%", (g/b - 1) * 100}')
  if [ "$GOT" -lt "$FLOOR" ]; then
    echo "  ✗ $k: $GOT vs ref $REF ($RATIO) — below floor $FLOOR"; STATUS=1
  else
    echo "  ✓ $k: $GOT vs ref $REF ($RATIO)"
  fi
done

# Print how far the box itself has moved since the baseline was recorded.
# This used to be invisible, and it was decided verdicts: the same code that
# measured 24.3M when the baseline was taken measured 21.0M three weeks
# later. Now the reference absorbs it, and the number is on the record.
echo "perfgate: box drift since the baseline was recorded (reference commit, same code):"
for k in $METRICS; do
  BASE=$(python3 -c "import json;print(json.load(open('$BASELINE'))['metrics'].get('$k',''))")
  REF=${REF_MED[$k]}
  [ -n "$BASE" ] && [ -n "$REF" ] && [ "$BASE" -ne 0 ] 2>/dev/null \
    && awk -v k="$k" -v b="$BASE" -v r="$REF" 'BEGIN{printf "    %-22s recorded %10d  now %10d  (%+.1f%%)\n", k, b, r, (r/b - 1) * 100}'
done

[ $STATUS -eq 0 ] && echo "perfgate: PASS" || echo "perfgate: FAIL — regression vs the reference commit; do not finish/release" >&2
exit $STATUS
