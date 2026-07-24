#!/usr/bin/env bash
# tiergate — the capacity-arc tiering gate (RFC 2026-07-24-v5-capacity-arc §8).
#
# One line per acceptance criterion; a line is either a real assertion or
# PENDING(<train>). The gate is RED until every line it owns is implemented
# and green — red-first is the point (crashgate precedent): the assertions
# exist before the feature, so the feature is built against them.
#
# Line ownership (fills in as trains land):
#   T3: L1 hot-p99, L3 spill-budget/stall, (B9/B12 live in
#       crates/kevy-embedded/tests/tier_transparency.rs, not here)
#   T4: L4 replay-with-spill, L10 rewrite-on-cold, L11 boot>budget
#   T5: L8 rss-budget/auto-probe
#   T9: L2 cold-read-p99, L5 vlog-amplification, L6 capacity-10x,
#       L12 D1-envelope, L13 hydration-batch, L14 mixed-isolation
set -euo pipefail

fail=0
line() { # name, status, detail
  local name="$1" status="$2" detail="$3"
  printf '%-24s %-12s %s\n' "$name" "$status" "$detail"
  if [ "$status" != "PASS" ]; then fail=1; fi
}

echo "tiergate — capacity-arc tiering acceptance (RFC §2 B/D groups)"
echo

line "L1  hot-p99 (B1)"        "PENDING(T3)" "hot GET/SET p99 delta <= noise across cold:hot sweeps"
line "L2  cold-read-p99 (B2)"  "PENDING(T9)" "scalar <=100us/300us, hash-row <=200us/500us on NVMe"
line "L3  spill-budget (B3)"   "PENDING(T3)" "sustained-ingest RAM plateau; spill stall p99 <= 1ms"
line "L4  replay-spill (B4)"   "PENDING(T4)" "replay-with-spill >= 0.70 x plain replay"
line "L5  vlog-amp (B5)"       "PENDING(T9)" "vlog_size <= 2.0 x cold_bytes after churn + compaction"
line "L6  capacity-10x (B6)"   "PENDING(T9)" "5M x 4KiB = 20GB on 2GB budget, op sweep green + RSS cap"
line "L8  rss-budget (B8)"     "PENDING(T5)" "RSS <= budget x 1.05 sustained; auto probe container+bare"
line "L10 rewrite-cold (B10)"  "PENDING(T4)" "BGREWRITEAOF on mostly-cold: digest equal + RAM bounded"
line "L11 boot>budget (B11)"   "PENDING(T4)" "replay of dataset>budget: RSS <= budget x 1.05 throughout"
line "L12 D1-envelope"         "PENDING(T9)" "10M x 1KiB on 3GB + 2 idx: C4/C5 hold, hydration p95<=10ms"
line "L13 hydration-batch (D3)" "PENDING(T6)" "one batched submission per page; preads == rows"
line "L14 mixed-isolation (D4)" "PENDING(T9)" "hot p99 unchanged under cold scan + backfill"

echo
if [ "$fail" -ne 0 ]; then
  echo "tiergate: RED — pending lines remain (expected until their trains land)"
  exit 1
fi
echo "tiergate: PASS"
