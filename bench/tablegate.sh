#!/usr/bin/env bash
# tablegate — the capacity-arc TABLE.*/RDS-views gate (RFC
# 2026-07-24-v5-capacity-arc §8, C group + D2).
#
# Red-first skeleton (crashgate precedent): one line per criterion, PENDING
# until its train lands. perf lines (C4/C5/C7) live in perfgate's METRICS
# list, not here; C8 formulas live in memgate/diskgate.
#
# Line ownership: T7: L1 conformance, L2 round-trip, L3 refusals, L6
# index-only counter; T9: L7 fully-cold index-only.
set -euo pipefail

fail=0
line() {
  local name="$1" status="$2" detail="$3"
  printf '%-26s %-12s %s\n' "$name" "$status" "$detail"
  if [ "$status" != "PASS" ]; then fail=1; fi
}

echo "tablegate — capacity-arc TABLE.* acceptance (RFC §2 C group)"
echo

line "L1 conformance (C1)"     "PENDING(T7)" "R1-R12 sequence: WHERE+FILTER/ORDER/OFFSET/COUNT/GROUP/unique/Via/txn/soft-del/seq"
line "L2 round-trip (C2)"      "PENDING(T7)" "TABLE.DECLARE -> derived idx/views, VERIFY fsck clean, oracle byte parity"
line "L3 refusals (C3)"        "PENDING(T7)" "ad-hoc SQL / query-time join / HAVING error by name, never silently"
line "L6 index-only (C6)"      "PENDING(T7)" "row-read counter == 0 for FILTER/SORT/COUNT queries (debug counters)"
line "L7 cold-index-only (D2)" "PENDING(T9)" "fully-cold table: cold-read counter == 0 on index-only queries"

echo
if [ "$fail" -ne 0 ]; then
  echo "tablegate: RED — pending lines remain (expected until their trains land)"
  exit 1
fi
echo "tablegate: PASS"
