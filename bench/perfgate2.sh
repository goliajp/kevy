#!/bin/bash
# perfgate2 — the perf regression gate on three orthogonal lines.
#
#   bash bench/perfgate2.sh <KEVY_BIN>                   # rolling reference, then release anchor
#   bash bench/perfgate2.sh <KEVY_BIN> --against rolling # one of the two
#   bash bench/perfgate2.sh <KEVY_BIN> --against anchor
#   bash bench/perfgate2.sh <KEVY_BIN> --calibrate       # A/A, writes sigma
#   bash bench/perfgate2.sh <KEVY_BIN> --mutant M1       # judging-power proof
#
# Lines, all read from the server process over the same window:
#   C  instructions:u / op, instructions:k / op, syscalls / op — the work per
#      command. Insensitive to neighbours, SMT load and clock; judged only on
#      samples where the server is saturated (idle spinning adds instructions).
#   S  cycles / op, and utilisation = task-clock / (window x server cores):
#      stalls, and sleeps or waits (a sleep adds no instructions).
#   T  ops/s — the product number; judged only when the whole box was quiet.
#   L  the hybrid-retrieval p95, one closed-loop client.
#
# Topology (lx64: CPU n and n+8 share a physical core): the server runs 4
# threads on 0-3, CPUs 8-11 stay empty, the load owns 4-7,12-15 with 8
# threads. The old 0-7 / 8-15 split paired every shard with a generator
# thread on the same core, and a spinning shard stole that thread's pipeline.
#
# Every window is checked before it counts: a busy sibling of a server core,
# foreign load on the server cores, or a load generator near its CPU limit
# discards it and it is retaken; six in a row refuse the run. Pairs of
# reference and candidate instances alternate (ABBA) and a sequential test
# on the paired log-ratios, with the noise sigma from --calibrate, decides
# each line; a red is re-tested on fresh pairs before it counts.
#
# Hardware counters come from `sudo -n /usr/local/sbin/kevy-perfstat PID SECS`.
# PERFGATE_LINES=T runs throughput only, and says it cannot judge 3%.
#
# Exit: 0 pass, 1 regression (or a mutant not proven), 2 refused,
# 3 undecided (the data does not separate the candidate from the band).
set -u
. "$(dirname "$0")/bench-lock.sh"   # hold the machine's bench lock for the whole run

BIN=${1:?usage: perfgate2.sh <KEVY_BIN> [--against rolling|anchor | --calibrate | --mutant NAME]}
MODE=${2:-both}
ARG=${3:-}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
BASELINE=$HERE/PERF-BASELINE2.json
LEDGER=$REPO/.dev/perf/ledger.jsonl
SRV_CPUS=0-3
SRV_THREADS=4
CLI_CPUS=4-7,12-15
CLI_THREADS=8
RAMP=${RAMP:-1}
WINDOW=${WINDOW:-3}          # whole seconds: kevy-perfstat takes an integer
WINDOWS=2                    # windows averaged into one observation
MAX_DISCARDS=6
CALIBRATION_PAIRS=10
LINES=all; [ "${PERFGATE_LINES:-}" = T ] && LINES=T
ANGLE_FILTER=${PERFGATE_ANGLES:-}

refuse() { echo "perfgate2: REFUSED — $1" >&2; exit 2; }

. "$HERE/perfgate-preflight.sh"
. "$HERE/perfgate-ref.sh"
. "$HERE/perfgate2-ref.sh"
. "$HERE/perfgate2-angles.sh"
. "$HERE/perfgate2-obs.sh"

# everything this gate spawns inherits the load cpus unless it pins itself;
# the server is the only thing that does
taskset -pc "$CLI_CPUS" $$ >/dev/null || refuse "cannot pin the gate to $CLI_CPUS"
trap 'angle_down; exit 2' INT TERM
FP=$(lscpu -e | judge fingerprint --baseline "$BASELINE")
[ -n "$FP" ] || refuse "no topology fingerprint (lscpu -e failed?)"

angles() {
  local all a
  all=$(python3 -c "import json;print(' '.join(json.load(open('$BASELINE'))['angles']))")
  for a in $all; do
    [ -z "$ANGLE_FILTER" ] || [[ " $ANGLE_FILTER " == *" $a "* ]] || continue
    [ "$JUDGE_LINES" = C ] && [[ $a == *_us ]] && continue
    printf "%s " "$a"
  done
}

# Measure every angle against REF_BIN and report. $1 = label, $2 = judge
# lines, $3 = extra report args. Returns the report's exit code.
run_against() {
  local label=$1 rc
  JUDGE_LINES=$2
  SAMPLES=$RUNDIR/samples-$label.jsonl
  : >"$SAMPLES"
  judge check --baseline "$BASELINE" --fingerprint "$FP" --angles "$(angles)" \
    --lines "$JUDGE_LINES" || exit 2
  for ANGLE in $(angles); do pairs_for_angle ""; done
  # shellcheck disable=SC2086 # $3 is zero or more flags
  judge report --samples "$SAMPLES" --baseline "$BASELINE" --fingerprint "$FP" \
    --angles "$(angles)" --ref "$label" --lines "$JUDGE_LINES" $3
  rc=$?
  [ "$rc" -eq 2 ] && exit 2
  judge ledger --samples "$SAMPLES" --baseline "$BASELINE" --out "$LEDGER" \
    --fingerprint "$FP" --commit "$(git -C "$REPO" rev-parse HEAD)" --ref "$label" >&2
  return $rc
}

need_bin() { # $1 = path or empty, $2 = what
  [ -n "$1" ] && [ -x "$1" ] || refuse "$2 unavailable (build failed?)"
}

calibrate() {
  REF_BIN=$BIN; CAND_BIN=$BIN; JUDGE_LINES=all
  SAMPLES=$RUNDIR/samples-calibrate.jsonl
  : >"$SAMPLES"
  echo "perfgate2: calibrating — A/A, $CALIBRATION_PAIRS pairs per angle, same binary both sides"
  for ANGLE in $(angles); do pairs_for_angle "$CALIBRATION_PAIRS"; done
  judge calibrate --samples "$SAMPLES" --baseline "$BASELINE" --fingerprint "$FP" \
    --angles "$(angles)" || exit 2
}

mutant() { # $1 = name
  local patch sha
  patch=$(mutant_patch "$1") || refuse "unknown mutant $1 (M1 M1b M2 M3 M4 M5)"
  sha=$(git -C "$REPO" rev-parse HEAD)
  echo "perfgate2: mutant $1 — both sides built from ${sha:0:12}; the binary argument is not used"
  REF_BIN=$(ref_binary "$sha"); need_bin "$REF_BIN" "reference build of ${sha:0:12}"
  CAND_BIN=$REF_BIN
  if [ -n "$patch" ]; then
    CAND_BIN=$(mutant_binary "$sha" "$patch"); need_bin "$CAND_BIN" "mutant $1"
  fi
  MUTANT=$1
  run_against "mutant-$1" "$LINES" "--mutant $1"
}

against_rolling() {
  local sha rc
  sha=$(rolling_ref)
  [ -n "$sha" ] || refuse "no rolling reference"
  REF_BIN=${PERFGATE_REF_BIN:-$(ref_binary "$sha")}; need_bin "$REF_BIN" "reference ${sha:0:12}"
  CAND_BIN=$BIN
  echo "perfgate2: candidate vs rolling reference ${sha:0:12}"
  run_against "${sha:0:12}" "$LINES" ""
  rc=$?
  if [ "$rc" -eq 0 ] && [ "$sha" = "$(python3 -c "import json;print(json.load(open('$BASELINE')).get('rolling_ref') or '')")" ] \
    && [ "$(git -C "$REPO" merge-base HEAD origin/develop)" = "$(git -C "$REPO" rev-parse HEAD)" ]; then
    judge advance --baseline "$BASELINE" --sha "$(git -C "$REPO" rev-parse HEAD)" --anchor "$(anchor_tag)"
  fi
  return $rc
}

against_anchor() {
  local tag
  tag=$(anchor_tag)
  [ -n "$tag" ] || refuse "no v* tag reachable from HEAD"
  [ "$LINES" = T ] && refuse "the anchor is judged on the C lines, which need counters"
  REF_BIN=$(ref_binary "$(git -C "$REPO" rev-parse "$tag^{commit}")"); need_bin "$REF_BIN" "anchor $tag"
  CAND_BIN=$BIN
  echo "perfgate2: candidate vs release anchor $tag (C lines only)"
  run_against "$tag" C ""
}

worst() { # exit codes ordered: 1 over 3 over 0
  if [ "$1" -eq 1 ] || [ "$2" -eq 1 ]; then echo 1
  elif [ "$1" -eq 3 ] || [ "$2" -eq 3 ]; then echo 3
  else echo 0; fi
}

case $MODE in
  --calibrate) calibrate ;;
  --mutant) [ -n "$ARG" ] || refuse "--mutant needs a name"; mutant "$ARG"; exit $? ;;
  --against)
    case $ARG in
      rolling) against_rolling; exit $? ;;
      anchor) against_anchor; exit $? ;;
      *) refuse "--against rolling|anchor" ;;
    esac ;;
  both)
    against_rolling; r1=$?
    against_anchor; r2=$?
    exit "$(worst "$r1" "$r2")" ;;
  *) refuse "unknown mode $MODE" ;;
esac
