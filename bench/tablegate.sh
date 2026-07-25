#!/usr/bin/env bash
# tablegate — the capacity-arc TABLE.*/RDS-views gate (RFC
# 2026-07-24-v5-capacity-arc §8, C group + D2).
#
# One line per criterion; a line is either a real assertion or
# PENDING(<train>). perf lines (C4/C5/C7) live in perfgate's METRICS
# list (baselines recorded on lx64), not here; C8 formulas live in
# memgate/diskgate.
#
# Line ownership: T7: L1 conformance, L2 round-trip, L3 refusals,
# L6 index-only counter, L7 cold-index-only (the c6 e2e asserts the
# cold-read counter == 0 on index-only queries — the D2 shape).
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
line() {
  local name="$1" status="$2" detail="$3"
  printf '%-26s %-12s %s\n' "$name" "$status" "$detail"
  if [ "$status" != "PASS" ]; then fail=1; fi
}

# Run one cargo test filter; echo PASS/FAIL (logs kept on failure).
run_t() { # $1 = package, $2 = test target, $3 = filter ("" = all)
  local log
  # NOTE: BSD mktemp refuses templates with a suffix after the Xs —
  # a .log suffix here created a LITERAL tablegate-XXXXXX.log once and
  # then failed every later call with "File exists" (false gate red).
  log=$(mktemp "${TMPDIR:-/tmp}/tablegate-XXXXXX")
  if cargo test -p "$1" --test "$2" $3 >"$log" 2>&1; then
    rm -f "$log"
    echo "PASS"
  else
    echo "FAIL(log: $log)"
  fi
}

echo "tablegate — capacity-arc TABLE.* acceptance (RFC §2 C group)"
echo

L1=$(run_t kevy table_e2e "")
line "L1 conformance (C1)"     "$L1" "R1-R12 sequence: WHERE+FILTER/ORDER/OFFSET/COUNT/unique/Via/txn/soft-del/seq (crates/kevy/tests/table_e2e.rs)"

L2A=$(run_t kevy-embedded dispatch_oracle "table_surface_matches_the_real_server")
L2B=$(run_t kevy table_e2e "c2_table_verify_clean")
if [ "$L2A" = PASS ] && [ "$L2B" = PASS ]; then L2=PASS; else L2="FAIL($L2A/$L2B)"; fi
line "L2 round-trip (C2)"      "$L2" "TABLE.DECLARE -> derived idx, VERIFY fsck clean, oracle byte parity server vs embedded"

L3=$(run_t kevy table_e2e "r9_c3_the_refusal_surface")
line "L3 refusals (C3)"        "$L3" "ad-hoc SQL / bad declarations / WHERE-on-non-composite error by name, never silently"

L6=$(run_t kevy table_e2e "c6_index_only_queries_touch_zero_cold_rows")
line "L6 index-only (C6)"      "$L6" "row-read counter == 0 for FILTER/SORT/COUNT on cold rows; FIELDS pays <= 1 read/row"

# L7 (D2): the same e2e asserts the cold-read counter == 0 on
# index-only queries over a mostly-cold table; the T9 envelope re-runs
# it at the fully-cold 10x scale on lx64.
line "L7 cold-index-only (D2)" "$L6" "cold rows (c6 fixture, mostly-cold): cold-read counter == 0 on index-only queries — alias of L6; T9 envelope re-runs it FULLY-cold at scale"

echo
if [ "$fail" -ne 0 ]; then
  echo "tablegate: RED — failing/pending lines remain"
  exit 1
fi
echo "tablegate: PASS"
