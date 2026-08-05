#!/usr/bin/env bash
# capacity-ceiling-sweep — find where the capacity envelope ENDS.
#
# bench/capacity-envelope.sh answers "does the declared contract hold?"
# and its B6 phase is sized to the contract: 5M x 4KiB on a 2GB budget
# IS 10x, so a pass reports ratio=10.0x/10x. That is a contract check.
# It does not say where the engine stops holding — and the sentence this
# is meant to underwrite ("this 8GB box holds 200GB") is 25x.
#
# So: hold the budget FIXED and grow the dataset. That is the shape of
# the product claim (a machine of a given size, more business on it), and
# it is the axis that loads the part of the model which scales with
# ENTRY COUNT — per-entry index metadata, fences, bloom filters — rather
# than with bytes. Shrinking the budget against a fixed dataset would
# raise the ratio without ever asking that question.
#
# Each rung reruns the full B6 phase (load, drain, RSS/used sampling,
# cold op sweep, B2 cold-read p99, B5 churn + amplification) and its
# results line is archived under its ratio. A rung that fails is the
# answer, not an error: stop and read which assertion gave.
#
# Usage (lx64, as kevybench, never root):
#   TMPDIR=$HOME/captmp bash bench/capacity-ceiling-sweep.sh [ratios…]
# Default rungs: 10 20 25 — baseline, midpoint, the claimed number.
#
#   KEVY_BIN  reuse a built binary (strongly recommended: otherwise each
#             rung rebuilds).
#   SWEEP_VAL value size, default 4096 (matches the B6 phase).
#   SWEEP_BUDGET_GB fixed budget per rung, default 2.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
OUT="$HERE/.capacity-ceiling-sweep"
VAL=${SWEEP_VAL:-4096}
BUDGET_GB=${SWEEP_BUDGET_GB:-2}
BUDGET=$((BUDGET_GB * 1024 ** 3))
RUNGS=${*:-10 20 25}

command -v python3 >/dev/null || { echo "sweep: needs python3" >&2; exit 1; }
[ "$(id -u)" -ne 0 ] || { echo "sweep: refusing to run as root" >&2; exit 1; }

{
  echo "# capacity-ceiling-sweep $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "# fixed budget ${BUDGET_GB}GB, value ${VAL}B, growing dataset"
} >"$OUT"

for R in $RUNGS; do
  # keys = ratio x budget / value, i.e. the dataset that makes data:RAM
  # exactly R:1.
  KEYS=$((R * BUDGET / VAL))
  DATA_GB=$((R * BUDGET_GB))
  echo
  echo "══ rung ${R}x — $KEYS keys x ${VAL}B = ${DATA_GB}GB on ${BUDGET_GB}GB ══"
  # Delete last rung's results first: a rung that dies before writing
  # would otherwise have the previous run's numbers read back as its own.
  rm -f "$HERE/.capacity-envelope-results-only-B6"
  env CAPACITY_ONLY=B6 \
      B6_KEYS_OVERRIDE="$KEYS" \
      B6_VAL_OVERRIDE="$VAL" \
      B6_BUDGET_OVERRIDE="$BUDGET" \
      bash "$HERE/capacity-envelope.sh"
  RC=$?
  LINE=$(grep '^L6=' "$HERE/.capacity-envelope-results-only-B6" 2>/dev/null)
  L2=$(grep '^L2=' "$HERE/.capacity-envelope-results-only-B6" 2>/dev/null)
  L5=$(grep '^L5=' "$HERE/.capacity-envelope-results-only-B6" 2>/dev/null)
  {
    echo "RUNG=${R}x rc=$RC keys=$KEYS data_gb=$DATA_GB budget_gb=$BUDGET_GB"
    echo "  ${LINE:-L6=<no results line>}"
    echo "  ${L2:-L2=<none>}"
    echo "  ${L5:-L5=<none>}"
  } >>"$OUT"
  cp -f "$HERE/.capacity-envelope-results-only-B6" \
        "$HERE/.capacity-envelope-results-${R}x" 2>/dev/null
  if [ $RC -ne 0 ]; then
    echo "sweep: rung ${R}x FAILED — that is the envelope's end, not an error." | tee -a "$OUT"
    echo "sweep: results -> $OUT"
    exit 0
  fi
done

echo
echo "sweep: every rung held (${RUNGS// /, }) — the ceiling is above the last one."
echo "sweep: results -> $OUT"
sed -n '3,$p' "$OUT" | sed 's/^/  /'
