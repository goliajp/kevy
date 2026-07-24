#!/usr/bin/env bash
# capacity-envelope — the T9 capacity-arc envelope runner (RFC
# 2026-07-24-v5-capacity-arc §2: B2 cold-read p99, B5 vlog space
# amplification, B6 capacity-10x, D1 fused envelope, D2 index-only,
# D3 hydration batching, D4 mixed-workload isolation — the tiergate
# lines L2 / L5 / L6 / L12 / L13 / L14).
#
# HOW TO RUN ON lx64 (the kevybench discipline — SLA numbers count only
# from there; never as root, never on a dirty box):
#
#   ssh kevybench@lx64
#   cd ~/kevy && git fetch origin && git checkout <T9 commit>
#   bash bench/capacity-envelope.sh                    # full scale, ~30-60 min
#   TIERGATE_RUN_ENVELOPE=1 bash bench/tiergate.sh     # consumes the results
#                                                      # file, flips L2/L5/L6/L12/L13/L14
#
# Env knobs:
#   KEVY_BIN        server binary (default: cargo build --release -p kevy here)
#   CAPACITY_SCALE  full (default) | tiny — tiny is the HARNESS PROOF scale:
#                   every phase runs and every scale-independent assertion is
#                   enforced (demotion engaged, index-only preads == 0,
#                   hydrate preads bounded per-row, zero promotions on bulk
#                   paths, the op sweep), but the RFC SLA thresholds are
#                   RECORDED, not judged — tiergate refuses to flip lines
#                   from a tiny run.
#   CAPACITY_PORT   default 6311; CAPACITY_KEEP=1 keeps scratch dirs.
#
# Results file: bench/.capacity-envelope-results (one line per tiergate
# line, machine-parsable; SCALE= header tells tiergate whether it counts).
#
# Workload shapes (all sized from the RFC capacity model, never asserted):
#   D1  10M rows x ~1KiB hashes on a 3GB budget, TABLE.DECLARE with a
#       VALUES index + a composite ORDERPATH (2 compiled indexes) —
#       C4-shape point lookup p99 (<=1ms), C5-shape FILTER+SORT page p95
#       (<=5ms), hydration page p95 (<=10ms), preads==rows, B2 hash-row
#       p99 (<=500us server e2e), D4 hot-p99 isolation under a digest
#       sweep + hydrate loop + index backfill.
#   B6  5M x 4KiB strings on a 2GB budget (>=10x data:RAM), RSS peak <=
#       budget x 1.05 throughout ingest, the cold op sweep, B2 scalar
#       cold-read p99 (<=300us server e2e), then a 25% overwrite churn
#       and the B5 vlog amplification bound (<=2.0x live cold bytes).
#
# Harness proof — tiny-scale results recorded from the dev host
# (2026-07-25, macOS, 2 shards, CAPACITY_SCALE=tiny). These numbers are
# NOT an SLA (wrong box, wrong scale) — they prove every phase of the
# harness runs end-to-end and every scale-independent assertion holds:
#   D1: 100k rows x ~1KiB on 64MB — cold_keys=72137; D2 index-only
#       preads delta=0; c4 p99=135us; c5 p95=280us; hydrate p95=346us,
#       preads=3967 <= 4000 (pages x 20 x shards; per-row, not
#       per-field), promotions=0, submissions=200 <= pages x shards;
#       coldrow p99=134us; D4 hot p99 96us -> 116us under
#       digest+hydrate+backfill.
#   B6: 100k x 4096B on 256MB — cold_keys=41047; RSS peak 279.1MB vs
#       cap 281.9MB; sweep 14/14 ok; coldget p99=111us; L5 churn amp
#       1.57x (<= 2.0x).
#   Exit: "harness OK (tiny scale — numbers recorded, NOT an SLA)".
#
# Exit codes: 0 = ran (full: all SLAs met), 1 = assertion/SLA failure,
# 2 = refused (root / dirty box / missing tools).
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
PY="$HERE/capacity_envelope.py"
SCALE=${CAPACITY_SCALE:-full}
PORT=${CAPACITY_PORT:-6311}
RESULTS="$HERE/.capacity-envelope-results"

refuse() { echo "capacity-envelope: REFUSED — $1" >&2; exit 2; }
fail()   { echo "capacity-envelope: FAIL — $1" >&2; exit 1; }
note()   { echo "capacity-envelope: $1"; }

# ---------- guards (the perfgate discipline) ----------
# Root can turn a cleanup foot-gun into an outage (the 2026-07 pkill
# incident); an envelope run never needs privilege. No override flag.
[ "$(id -u)" -ne 0 ] || refuse "refusing to run as root — use an unprivileged \
bench account (lx64: kevybench, checkout ~/kevy)"
command -v python3 >/dev/null || refuse "python3 not installed"
[ -f "$PY" ] || refuse "driver $PY missing"
case "$SCALE" in full|tiny) ;; *) refuse "CAPACITY_SCALE must be full|tiny" ;; esac
# Leftover servers / load generators pollute every number. The pattern
# matches kevy-as-a-command (".../kevy --port" or bare "kevy"), not
# paths that merely contain the repo name.
LEFTOVER=$(ps ax -o pid=,command= | grep -E "(^|/)kevy( |$)|redis-benchmark" \
  | grep -v grep | grep -v capacity-envelope || true)
[ -n "$LEFTOVER" ] && refuse "leftover bench processes (sweep first):
$LEFTOVER"
if [ "$SCALE" = full ]; then
  [ "$(uname)" = Linux ] || refuse "full scale is the lx64 measurement — Linux only"
  # Instantaneous idle%, not loadavg (same rationale as perfgate).
  read -r _ u1 n1 s1 i1 _ < /proc/stat; sleep 1; read -r _ u2 n2 s2 i2 _ < /proc/stat
  IDLE=$(( (i2 - i1) * 100 / ( (u2-u1) + (n2-n1) + (s2-s1) + (i2-i1) ) ))
  [ "$IDLE" -ge 80 ] || refuse "box busy (idle ${IDLE}% < 80%)"
fi

# ---------- scale parameters ----------
if [ "$SCALE" = full ]; then
  D1_ROWS=10000000;  D1_BUDGET=$((3 * 1024 ** 3)); THREADS=8
  B6_KEYS=5000000;   B6_VAL=4096; B6_BUDGET=$((2 * 1024 ** 3))
  SAMPLES=2000; PAGES=500; DRAIN=30; ENFORCE=1
else
  # Tiny: dataset must still exceed the budget so demotion genuinely
  # engages (D1: ~100MB data vs 64MB; B6: ~410MB vs 256MB — the B6
  # budget stays above an empty server's baseline RSS so the RSS
  # record means something even unenforced).
  D1_ROWS=100000;    D1_BUDGET=$((64 * 1024 ** 2)); THREADS=2
  B6_KEYS=100000;    B6_VAL=4096; B6_BUDGET=$((256 * 1024 ** 2))
  SAMPLES=300; PAGES=100; DRAIN=6; ENFORCE=0
fi

# ---------- binary ----------
if [ -z "${KEVY_BIN:-}" ]; then
  note "building release kevy (set KEVY_BIN to skip)"
  ( cd "$HERE/.." && cargo build -q --release -p kevy --bin kevy ) \
    || refuse "release build failed"
  KEVY_BIN="$HERE/../target/release/kevy"
fi
[ -x "$KEVY_BIN" ] || refuse "$KEVY_BIN is not executable"

# ---------- scratch + cleanup (hygiene rule: trap-clean, keep on failure) ----------
RUNDIR=$(mktemp -d "${TMPDIR:-/tmp}/capenv-XXXXXX")
SRV=""; BGPIDS=""
on_exit() {
  local rc=$?
  [ -n "$SRV" ] && kill "$SRV" 2>/dev/null
  for p in $BGPIDS; do kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  if [ $rc -eq 0 ] && [ "${CAPACITY_KEEP:-0}" != 1 ]; then rm -rf "$RUNDIR"
  else echo "capacity-envelope: scratch kept at $RUNDIR" >&2; fi
}
trap on_exit EXIT

kcmd()  { python3 "$PY" cmd  --port "$PORT" -- "$@"; }
tinfo() { local v; v=$(python3 "$PY" info --port "$PORT" --field "$1"); echo "${v:-0}"; }
rss_kb() { # portable RSS of pid $1, in KB
  if [ -r "/proc/$1/status" ]; then awk '/^VmRSS:/{print $2}' "/proc/$1/status"
  else ps -o rss= -p "$1" 2>/dev/null | tr -d ' '; fi
}
p_of() { sed -nE "s/.*${2}_us=([0-9]+).*/\1/p" <<<"$1"; }

server_start() { # $1 = budget bytes, $2 = data dir
  env KEVY_TIER_BUDGET="$1" KEVY_BIND=127.0.0.1 \
    "$KEVY_BIN" --port "$PORT" --threads "$THREADS" --dir "$2" --no-aof \
    >"$RUNDIR/srv-$(basename "$2").log" 2>&1 &
  SRV=$!
  for _ in $(seq 1 150); do
    [ "$(kcmd PING 2>/dev/null)" = "+PONG" ] && return 0
    kill -0 "$SRV" 2>/dev/null || break
    sleep 0.2
  done
  refuse "server did not come up (see $RUNDIR/srv-*.log)"
}
server_stop() {
  [ -n "$SRV" ] && { kill "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; }
  SRV=""
}

# Verdict plumbing. assert_always trips at EVERY scale (these prove the
# harness/mechanism); sla() enforces only at full — at tiny it records.
FAILED=0
assert_always() { # $1 desc, $2 = 0/1 ok
  if [ "$2" = 1 ]; then echo "  [ok]   $1"
  else echo "  [FAIL] $1"; FAILED=1; fi
}
sla() { # $1 desc, $2 = 0/1 ok -> echoes PASS/FAIL/RECORDED and sets SLA_LAST
  if [ "$ENFORCE" = 1 ]; then
    if [ "$2" = 1 ]; then SLA_LAST=PASS; echo "  [PASS] $1"
    else SLA_LAST=FAIL; FAILED=1; echo "  [FAIL] $1"; fi
  else
    SLA_LAST=RECORDED; echo "  [rec]  $1 (tiny scale — recorded, not judged)"
  fi
}
combine() { # PASS iff every arg is PASS; RECORDED if any RECORDED; else FAIL
  local out=PASS a
  for a in "$@"; do
    [ "$a" = RECORDED ] && { out=RECORDED; }
  done
  if [ "$out" != RECORDED ]; then
    for a in "$@"; do [ "$a" = PASS ] || out=FAIL; done
  fi
  echo "$out"
}

echo "capacity-envelope: scale=$SCALE bin=$(basename "$KEVY_BIN") port=$PORT threads=$THREADS"
echo

# ════════════════════════ Phase D1 — the fused envelope ════════════════════════
note "phase D1: $D1_ROWS rows x ~1KiB on $((D1_BUDGET / 1024 / 1024))MB budget"
D1DIR=$(mktemp -d "${TMPDIR:-/tmp}/capenv-d1-XXXXXX")
server_start "$D1_BUDGET" "$D1DIR"
python3 "$PY" load-d1 --port "$PORT" --rows "$D1_ROWS" --pad 900 --seed 1 || fail "D1 load"

# Declare AFTER the load (cookbook §15's bulk-load rule) — the backfill
# sweeps the already-cold rows through the no-promote peek.
DECL=$(kcmd TABLE.DECLARE env PREFIX row: PK id \
  COLUMN id i64 COLUMN status str COLUMN score i64 COLUMN ts i64 \
  INDEX score range VALUES status ts \
  ORDERPATH by_status_ts ON status THEN ts DESC)
[ "$DECL" = "+OK" ] || fail "TABLE.DECLARE: $DECL"
note "waiting for backfill (both compiled indexes)"
for probe in "IDX.QUERY env.score EQ 0" "IDX.QUERY env.by_status_ts WHERE status EQ s0 LIMIT 1"; do
  for _ in $(seq 1 1800); do
    # shellcheck disable=SC2086
    R=$(kcmd $probe)
    case "$R" in -INDEXBUILDING*) sleep 1 ;; -*) fail "backfill probe: $R" ;; *) break ;; esac
  done
done
sleep 2  # let the INFO gauge tick refresh
COLD_KEYS=$(tinfo cold_keys); PROM0=$(tinfo promotions_total)
note "post-load gauges: cold_keys=$COLD_KEYS cold_bytes=$(tinfo cold_bytes) stub_bytes=$(tinfo stub_bytes) vlog=$(tinfo vlog_size_bytes)"
assert_always "demotion engaged (cold_keys=$COLD_KEYS > 0)" "$([ "$COLD_KEYS" -gt 0 ] && echo 1 || echo 0)"

# D2 — index-only queries on a mostly-cold table touch zero cold rows.
P0=$(tinfo peek_preads_total)
WOUT=$(python3 "$PY" lat --port "$PORT" --n "$SAMPLES" --rows "$D1_ROWS" --seed 11 --mode where) || fail "where lat"
echo "  $WOUT"
sleep 2; P1=$(tinfo peek_preads_total)
assert_always "D2: index-only WHERE pages paid 0 cold reads (preads delta=$((P1 - P0)))" \
  "$([ $((P1 - P0)) -eq 0 ] && echo 1 || echo 0)"

# C4 / C5 shapes (perfgate's table_* baselines are ALSO recorded on this
# box — these are the envelope's own latency records).
C4OUT=$(python3 "$PY" lat --port "$PORT" --n "$SAMPLES" --rows "$D1_ROWS" --seed 12 --mode c4) || fail "c4 lat"
echo "  $C4OUT"; C4_P99=$(p_of "$C4OUT" p99)
sla "C4-shape point lookup p99 = ${C4_P99}us (target <= 1000us)" \
  "$([ "$C4_P99" -le 1000 ] && echo 1 || echo 0)"; V_C4=$SLA_LAST
C5OUT=$(python3 "$PY" lat --port "$PORT" --n "$SAMPLES" --rows "$D1_ROWS" --seed 13 --mode c5) || fail "c5 lat"
echo "  $C5OUT"; C5_P95=$(p_of "$C5OUT" p95)
sla "C5-shape FILTER+SORT page p95 = ${C5_P95}us (target <= 5000us)" \
  "$([ "$C5_P95" -le 5000 ] && echo 1 || echo 0)"; V_C5=$SLA_LAST

# L12/L13 — hydration over cold pages: p95, preads==rows (per ROW, not
# per field: 3 fields requested, so a per-field regression would read 3x
# the bound), zero promotions, batched submissions.
sleep 2
P0=$(tinfo peek_preads_total); S0=$(tinfo batch_submissions_total); PR0=$(tinfo promotions_total)
HOUT=$(python3 "$PY" lat --port "$PORT" --n "$PAGES" --rows "$D1_ROWS" --seed 14 --mode hydrate) || fail "hydrate lat"
echo "  $HOUT"; HYD_P95=$(p_of "$HOUT" p95)
sleep 2
PD=$(( $(tinfo peek_preads_total) - P0 )); SD=$(( $(tinfo batch_submissions_total) - S0 ))
PRD=$(( $(tinfo promotions_total) - PR0 ))
# Rows-hydrated upper bound: each SHARD hydrates up to LIMIT candidate
# rows before the origin truncates the merged page (fan-out over-fetch,
# measured 2x at 2 shards on the tiny smoke) — so per-ROW means
# <= pages x LIMIT x shards. A per-FIELD regression (3 fields
# requested) would read 3x this bound.
ROWS_UP=$((PAGES * 20 * THREADS))
note "hydrate counters: preads=$PD (row bound $ROWS_UP = pages x 20 x shards), submissions=$SD (pages=$PAGES, shards=$THREADS), promotions=$PRD"
assert_always "hydration paid cold reads (preads=$PD > 0)" "$([ "$PD" -gt 0 ] && echo 1 || echo 0)"
assert_always "one pread per ROW, not per field (preads=$PD <= $ROWS_UP)" \
  "$([ "$PD" -le "$ROWS_UP" ] && echo 1 || echo 0)"
assert_always "hydration promoted nothing (delta=$PRD)" "$([ "$PRD" -eq 0 ] && echo 1 || echo 0)"
assert_always "hydration batched (1 <= submissions=$SD <= pages x shards = $((PAGES * THREADS)))" \
  "$([ "$SD" -ge 1 ] && [ "$SD" -le $((PAGES * THREADS)) ] && echo 1 || echo 0)"
sla "D1 hydration page p95 = ${HYD_P95}us (target <= 10000us)" \
  "$([ "$HYD_P95" -le 10000 ] && echo 1 || echo 0)"; V_HYD=$SLA_LAST

# B2 (hash half) — whole-row materialization on distinct cold rows.
CROUT=$(python3 "$PY" lat --port "$PORT" --n "$SAMPLES" --rows "$D1_ROWS" --seed 15 --mode coldrow) || fail "coldrow lat"
echo "  $CROUT"; HASH_P99=$(p_of "$CROUT" p99)
sla "B2 cold hash-row p99 = ${HASH_P99}us (target <= 500us server e2e)" \
  "$([ "$HASH_P99" -le 500 ] && echo 1 || echo 0)"; V_B2H=$SLA_LAST

# L14 / D4 — hot p99 while a digest sweep + a hydrate loop + an index
# backfill chew the cold tier. Warmup promotes the narrow hot set first
# (promotion is on the second access).
python3 "$PY" lat --port "$PORT" --n "$SAMPLES" --rows "$D1_ROWS" --seed 16 --mode hot >/dev/null || fail "hot warmup"
HB=$(python3 "$PY" lat --port "$PORT" --n "$SAMPLES" --rows "$D1_ROWS" --seed 17 --mode hot) || fail "hot baseline"
echo "  baseline   $HB"; HOT0=$(p_of "$HB" p99)
python3 "$PY" lat --port "$PORT" --n 1 --rows "$D1_ROWS" --seed 18 --mode digest >"$RUNDIR/digest.out" 2>&1 &
BGPIDS="$BGPIDS $!"
python3 "$PY" lat --port "$PORT" --n 100000 --rows "$D1_ROWS" --seed 19 --mode hydrate >/dev/null 2>&1 &
BGPIDS="$BGPIDS $!"
BF=$(kcmd IDX.CREATE d4bf ON PREFIX row: FIELD ts TYPE i64 KIND range)
case "$BF" in
  +OK) BF_NOTE="backfill running" ;;
  -ERR*floor*) BF_NOTE="backfill refused by the tier index floor (documented discipline); digest+hydrate are the bulk load" ;;
  *) fail "IDX.CREATE d4bf: $BF" ;;
esac
sleep 2
HC=$(python3 "$PY" lat --port "$PORT" --n "$SAMPLES" --rows "$D1_ROWS" --seed 20 --mode hot) || fail "hot concurrent"
echo "  concurrent $HC ($BF_NOTE)"; HOT1=$(p_of "$HC" p99)
for p in $BGPIDS; do kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; done; BGPIDS=""
# "Unchanged" is the criterion; 2x is the mechanical trip-wire — the
# recorded pair is the deliverable the ledger reads.
sla "D4 hot p99 ${HOT0}us -> ${HOT1}us under cold sweep + backfill (trip-wire <= 2x)" \
  "$([ "$HOT1" -le $((HOT0 * 2)) ] && echo 1 || echo 0)"; V_D4=$SLA_LAST
server_stop
[ "${CAPACITY_KEEP:-0}" = 1 ] || rm -rf "$D1DIR"
echo

# ════════════════════════ Phase B6 — capacity 10x + B2 scalar + B5 ════════════════════════
note "phase B6: $B6_KEYS x ${B6_VAL}B on $((B6_BUDGET / 1024 / 1024))MB budget"
B6DIR=$(mktemp -d "${TMPDIR:-/tmp}/capenv-b6-XXXXXX")
server_start "$B6_BUDGET" "$B6DIR"
: >"$RUNDIR/rss.samples"
( while kill -0 "$SRV" 2>/dev/null; do rss_kb "$SRV" >>"$RUNDIR/rss.samples"; sleep 0.5; done ) &
SAMP=$!; BGPIDS="$SAMP"
python3 "$PY" load-b6 --port "$PORT" --keys "$B6_KEYS" --val "$B6_VAL" --seed 2 || fail "B6 load"
sleep "$DRAIN"  # drain the spill backlog, keep sampling
PEAK_KB=$(sort -n "$RUNDIR/rss.samples" | tail -1); PEAK=$((${PEAK_KB:-0} * 1024))
CAP=$((B6_BUDGET * 105 / 100))
RATIO=$(awk -v k="$B6_KEYS" -v v="$B6_VAL" -v b="$B6_BUDGET" 'BEGIN{printf "%.1f", k*v/b}')
COLD_KEYS=$(tinfo cold_keys)
note "gauges: cold_keys=$COLD_KEYS vlog=$(tinfo vlog_size_bytes) rss_peak=$PEAK cap=$CAP data:RAM=${RATIO}x"
assert_always "demotion engaged (cold_keys=$COLD_KEYS > 0)" "$([ "$COLD_KEYS" -gt 0 ] && echo 1 || echo 0)"
sla "B6 RSS peak $PEAK <= budget x 1.05 = $CAP" "$([ "$PEAK" -le "$CAP" ] && echo 1 || echo 0)"; V_RSS=$SLA_LAST
sla "B6 data:RAM ratio ${RATIO}x (gate >= 10x @ 4KiB values)" \
  "$(awk -v r="$RATIO" 'BEGIN{print (r >= 10) ? 1 : 0}')"; V_RATIO=$SLA_LAST
if python3 "$PY" sweep --port "$PORT" --keys "$B6_KEYS"; then SWEEP_OK=ok
else SWEEP_OK=FAILED; FAILED=1; fi

# B2 (scalar half) — distinct cold keys, one read each.
CGOUT=$(python3 "$PY" lat --port "$PORT" --n "$SAMPLES" --rows "$B6_KEYS" --seed 21 --mode coldget) || fail "coldget lat"
echo "  $CGOUT"; SCALAR_P99=$(p_of "$CGOUT" p99)
sla "B2 cold scalar p99 = ${SCALAR_P99}us (target <= 300us server e2e)" \
  "$([ "$SCALAR_P99" -le 300 ] && echo 1 || echo 0)"; V_B2S=$SLA_LAST

# L5 — churn 25% of the keys (fresh bytes -> dead vlog records), let
# compaction run on the ticks, then bound space amplification.
note "L5 churn: overwriting first 25% of keys"
python3 "$PY" load-b6 --port "$PORT" --keys $((B6_KEYS / 4)) --val "$B6_VAL" --seed 3 || fail "churn load"
PREV=-1
for _ in $(seq 1 40); do
  sleep 3
  CUR=$(tinfo vlog_size_bytes)
  [ "$CUR" = "$PREV" ] && break
  PREV=$CUR
done
VLOG=$(tinfo vlog_size_bytes); CBYTES=$(tinfo cold_bytes)
AMP=$(awk -v s="$VLOG" -v c="$CBYTES" 'BEGIN{ if (c == 0) print "inf"; else printf "%.2f", s / c }')
note "L5 gauges: vlog_size=$VLOG cold_bytes=$CBYTES amp=${AMP}x (epoch=$(tinfo vlog_epoch))"
sla "B5 vlog amplification ${AMP}x <= 2.0x live cold bytes" \
  "$(awk -v a="$AMP" 'BEGIN{print (a != "inf" && a <= 2.0) ? 1 : 0}')"; V_AMP=$SLA_LAST
kill "$SAMP" 2>/dev/null; BGPIDS=""
server_stop
[ "${CAPACITY_KEEP:-0}" = 1 ] || rm -rf "$B6DIR"
echo

# ════════════════════════ results file (tiergate wiring) ════════════════════════
L2=$(combine "$V_B2S" "$V_B2H"); L5=$V_AMP
L6=$(combine "$V_RSS" "$V_RATIO")
L12=$(combine "$V_C4" "$V_C5" "$V_HYD"); L13=$V_HYD; L14=$V_D4
# The mechanism asserts (preads/promotions/sweep) are assert_always —
# any failure there already set FAILED; fold it into the lines they own.
if [ "$FAILED" = 1 ] && [ "$ENFORCE" = 1 ]; then L12=FAIL; L13=FAIL; L6=FAIL; fi
{
  echo "# generated by bench/capacity-envelope.sh $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "SCALE=$SCALE"
  echo "L2=$L2 scalar_p99us=$SCALAR_P99/300 hash_row_p99us=$HASH_P99/500"
  echo "L5=$L5 vlog=$VLOG cold_bytes=$CBYTES amp=${AMP}x/2.0x"
  echo "L6=$L6 ratio=${RATIO}x/10x rss_peak=$PEAK cap=$CAP sweep=$SWEEP_OK"
  echo "L12=$L12 c4_p99us=$C4_P99/1000 c5_p95us=$C5_P95/5000 hyd_p95us=$HYD_P95/10000 preads=$PD<=rows=$ROWS_UP"
  echo "L13=$L13 submissions=$SD pages=$PAGES shards=$THREADS preads=$PD"
  echo "L14=$L14 hot_p99us=$HOT0->$HOT1 under digest+hydrate+backfill ($BF_NOTE)"
} >"$RESULTS"
echo "capacity-envelope: results -> $RESULTS"
sed -n '2,$p' "$RESULTS" | sed 's/^/  /'
echo
if [ "$FAILED" = 1 ]; then
  echo "capacity-envelope: FAIL — see the [FAIL] lines above" >&2
  exit 1
fi
if [ "$ENFORCE" = 1 ]; then echo "capacity-envelope: PASS (full scale — tiergate may consume the results)"
else echo "capacity-envelope: harness OK (tiny scale — numbers recorded, NOT an SLA; tiergate stays pending)"; fi
