#!/usr/bin/env bash
# perfgate-median — N full perfgate runs, per-angle medians, one verdict.
#
# Why this exists: single perfgate runs carry a ±3-6 pp per-angle band
# (two same-tip runs disagreed by that much on every collection angle —
# the owner thread), so no single
# run can green or red an angle inside it. The methodology's bench-infra
# clause says: invest in the instrument, not in re-arguing its noise.
# It became practical when the zadd stall (which REFUSED half the runs)
# was fixed.
#
#   bash bench/perfgate-median.sh <KEVY_BIN> [N]
#
# Runs perfgate N times (default 3), collects each angle's candidate
# and reference samples, and judges each angle on median(cand) vs
# median(ref) with perfgate's own 0.92 floor. A REFUSED run aborts the
# whole gate: medians over a partial set are not medians.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${1:?usage: perfgate-median.sh <KEVY_BIN> [N]}
N=${2:-3}
FLOOR=${PERFGATE_FLOOR:-0.92}
OUT=$(mktemp -d "${TMPDIR:-/tmp}/pgmed-XXXXXX")
trap 'rm -rf "$OUT"' EXIT

for i in $(seq 1 "$N"); do
  echo "perfgate-median: run $i/$N"
  if ! bash "$HERE/perfgate.sh" "$BIN" >"$OUT/run$i" 2>&1; then
    # FAIL exit is fine (angles below floor still measured); REFUSED is not.
    if grep -q "REFUSED" "$OUT/run$i"; then
      echo "perfgate-median: run $i REFUSED — aborting (a median over a partial set is not a median)" >&2
      grep -m1 "REFUSED" "$OUT/run$i" >&2
      exit 2
    fi
  fi
  grep -E "  [✓✗] " "$OUT/run$i" | sed -E 's/^  [✓✗] ([a-z0-9_]+): ([0-9]+) vs ref ([0-9]+).*/\1 \2 \3/' >>"$OUT/samples"
done

median_col() { # $1 = angle, $2 = column (2=cand, 3=ref)
  awk -v a="$1" -v col="$2" '$1==a {print $col}' "$OUT/samples" \
    | sort -n | awk '{v[NR]=$1} END {print v[int((NR+1)/2)]}'
}

echo "perfgate-median: verdict on per-angle medians of $N runs (floor = ref x $FLOOR)"
fail=0
for angle in $(awk '{print $1}' "$OUT/samples" | sort -u); do
  c=$(median_col "$angle" 2)
  r=$(median_col "$angle" 3)
  n=$(awk -v a="$angle" '$1==a' "$OUT/samples" | wc -l | tr -d ' ')
  verdict=$(awk -v c="$c" -v r="$r" -v f="$FLOOR" \
    'BEGIN {print (c >= r*f) ? "PASS" : "FAIL"}')
  [ "$verdict" = FAIL ] && fail=1
  awk -v a="$angle" -v c="$c" -v r="$r" -v v="$verdict" -v n="$n" \
    'BEGIN {printf "  %-24s %s  median %d vs ref %d (%+.1f%%, n=%d)\n", a, v, c, r, 100*(c/r-1), n}'
done
[ "$fail" -eq 0 ] && echo "perfgate-median: PASS" || echo "perfgate-median: FAIL — at least one angle below floor on medians"
exit "$fail"
