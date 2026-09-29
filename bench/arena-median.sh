#!/usr/bin/env bash
# arena-median — N clean arena rounds, per-cell medians, paired intervals.
#
# Why this exists: a single arena round cannot separate two engines inside
# its own spread. On 2026-09-01 three rounds of one unchanged binary
# disagreed by 26.6% on SADD and 13% on INCR; within-round stdev over five
# windows does not predict that. And a round taken while something else had
# the box is not a sample of the engine at all: on 2026-09-28 one such round
# moved Redis SET by 35.6% and was judged instead of discarded.
#
#   bash bench/arena-median.sh <KEVY_BIN> [N]
#
# Keeps N rounds that arena calls clean (no window with more than 10% foreign
# load on the box), retaking dirty ones up to 2N attempts. The table is the
# per-cell median over every window of every clean round, and each ratio is a
# 99% paired bootstrap interval over those windows (round and window slot
# paired): an interval that contains 1 is NOISE, and its lower bound is the
# ratio that can be claimed.
#
# Exit: 0 = every cell's interval lies above 1 against every competitor;
# 1 = some cell's does not (say so in the ledger rather than quoting the
# median); 2 = a round failed or too many were dirty.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${1:?usage: arena-median.sh <KEVY_BIN> [N]}
N=${2:-3}
OUT=$(mktemp -d "${TMPDIR:-/tmp}/armed-XXXXXX")
trap 'rm -rf "$OUT"' EXIT

clean=()
attempt=0
while [ "${#clean[@]}" -lt "$N" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -gt $((2 * N)) ]; then
        echo "arena-median: only ${#clean[@]} clean rounds in $((2 * N)) attempts — the box is busy" >&2
        exit 2
    fi
    echo "arena-median: round $attempt (${#clean[@]}/$N clean so far)" >&2
    ARENA_SAMPLES=$OUT/run$attempt.jsonl bash "$HERE/arena.sh" "$BIN" >"$OUT/run$attempt" 2>"$OUT/err$attempt"
    rc=$?
    case $rc in
        0) clean+=("$OUT/run$attempt.jsonl") ;;
        3) echo "arena-median: round $attempt dirty — discarded" >&2
           grep -h "DIRTY\|dirty round" "$OUT/run$attempt" "$OUT/err$attempt" >&2 ;;
        *) echo "arena-median: round $attempt failed (exit $rc)" >&2
           tail -3 "$OUT/err$attempt" >&2
           exit 2 ;;
    esac
done
head -20 "$OUT/run$attempt" | grep '^#'
python3 "$HERE/arena_table.py" median "$HERE/PERF-BASELINE2.json" \
    "$HERE/COMPETITOR-ANCHORS.json" "${clean[@]}"
