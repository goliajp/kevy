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
#       L12 D1-envelope, L13 hydration-batch (mechanism landed in T6;
#       the envelope-scale measurement is T9's), L14 mixed-isolation
set -euo pipefail

fail=0
line() { # name, status, detail
  local name="$1" status="$2" detail="$3"
  printf '%-24s %-12s %s\n' "$name" "$status" "$detail"
  if [ "$status" != "PASS" ]; then fail=1; fi
}

# ── T9 envelope wiring: bench/capacity-envelope.sh writes one verdict
# line per gate line it owns into bench/.capacity-envelope-results.
# With TIERGATE_RUN_ENVELOPE=1 those verdicts render here (the same
# results-from-a-runner pattern tablegate's run_t uses for cargo).
# Only a SCALE=full run counts — the kevybench discipline: a tiny-scale
# (harness-proof) results file flips nothing.
ENV_RESULTS=${TIERGATE_ENVELOPE_RESULTS:-$(cd "$(dirname "$0")" && pwd)/.capacity-envelope-results}
env_ok=0
if [ "${TIERGATE_RUN_ENVELOPE:-0}" = 1 ] && [ -f "$ENV_RESULTS" ] \
   && grep -q '^SCALE=full$' "$ENV_RESULTS"; then
  env_ok=1
fi
env_line() { # $1 = results key (L2…), $2 = display name, $3 = pending detail
  local rec
  if [ "$env_ok" = 1 ]; then
    rec=$(sed -n "s/^$1=//p" "$ENV_RESULTS")
    if [ -n "$rec" ]; then
      line "$2" "${rec%% *}" "${rec#* }"
      return
    fi
  fi
  line "$2" "PENDING(T9)" "$3 [runner: capacity-envelope.sh on lx64, then TIERGATE_RUN_ENVELOPE=1]"
}

# ── L8 assertion body (T5): RSS ≤ budget × 1.05 sustained + the auto
# probe smoke. The SHAPE lands with T5; the line stays PENDING until a
# tiered server run exists on lx64 (kevybench discipline) — run it
# there with TIERGATE_RUN_L8=1 KEVY_BIN=<path>. Linux-only (/proc RSS).
l8_rss_budget() { # -> "PASS" or "FAIL: why" on stdout
  local bin=${KEVY_BIN:?TIERGATE_RUN_L8 needs KEVY_BIN}
  local port=${TIERGATE_PORT:-6301}
  local budget_bytes=$((256 * 1024 * 1024)) # 256mb budget, ~1 GB dataset
  local dir; dir=$(mktemp -d)
  KEVY_TIER_BUDGET=256mb "$bin" --port "$port" --threads 1 --dir "$dir" &>/dev/null &
  local srv=$!
  # shellcheck disable=SC2064
  trap "kill $srv 2>/dev/null; sleep 0.2; kill -9 $srv 2>/dev/null; wait $srv 2>/dev/null; rm -rf '$dir'" RETURN
  sleep 1
  # Auto-probe smoke: the resolved budget gauge must be present + sane.
  local gauge
  gauge=$(redis-cli -p "$port" info tiering | tr -d '\r' | awk -F: '/^tier_budget_bytes:/{print $2}')
  [ "${gauge:-0}" -eq "$budget_bytes" ] || { echo "FAIL: tier_budget_bytes=$gauge != $budget_bytes"; return 0; }
  # Sustained ingest past the budget; sample RSS throughout.
  local cap=$((budget_bytes * 105 / 100)) peak=0 rss
  redis-benchmark -p "$port" -t set -n 250000 -r 250000 -d 4096 -q >/dev/null 2>&1 &
  local bench=$!
  while kill -0 "$bench" 2>/dev/null; do
    rss=$(( $(awk '/^VmRSS:/{print $2}' "/proc/$srv/status" 2>/dev/null || echo 0) * 1024 ))
    [ "$rss" -gt "$peak" ] && peak=$rss
    sleep 0.5
  done
  wait "$bench" 2>/dev/null || true
  sleep 2 # drain the spill backlog, then one final sample
  rss=$(( $(awk '/^VmRSS:/{print $2}' "/proc/$srv/status" 2>/dev/null || echo 0) * 1024 ))
  [ "$rss" -gt "$peak" ] && peak=$rss
  if [ "$peak" -gt "$cap" ]; then
    echo "FAIL: peak RSS $peak > budget×1.05 $cap"
  else
    echo "PASS"
  fi
}

echo "tiergate — capacity-arc tiering acceptance (RFC §2 B/D groups)"
echo

line "L1  hot-p99 (B1)"        "PENDING(T9/lx64)" "mechanics landed (T3, unit-tested); the p99 sweep itself runs on lx64 via perfgate + envelope"
env_line L2 "L2  cold-read-p99 (B2)"  "scalar <=100us/300us, hash-row <=200us/500us on NVMe"
line "L3  spill-budget (B3)"   "PENDING(T9/lx64)" "batching/hysteresis landed (T3, unit-tested); the stall-p99 measurement runs on lx64"
line "L4  replay-spill (B4)"   "PENDING(T4)" "replay-with-spill >= 0.70 x plain replay"
env_line L5 "L5  vlog-amp (B5)"       "vlog_size <= 2.0 x cold_bytes after churn + compaction"
env_line L6 "L6  capacity-10x (B6)"   "5M x 4KiB = 20GB on 2GB budget, op sweep green + RSS cap"
# L8: the assertion body is implemented (T5, above) but a tiered
# SERVER run only exists on lx64 — the line stays PENDING-red until the
# T9 close-out runs it there (TIERGATE_RUN_L8=1 KEVY_BIN=… flips it).
if [ "${TIERGATE_RUN_L8:-0}" = "1" ]; then
  l8_verdict=$(l8_rss_budget)
  if [ "$l8_verdict" = "PASS" ]; then
    line "L8  rss-budget (B8)"   "PASS"        "RSS <= budget x 1.05 sustained; auto probe gauge sane"
  else
    line "L8  rss-budget (B8)"   "FAIL"        "$l8_verdict"
  fi
else
  line "L8  rss-budget (B8)"     "PENDING(T5)" "body landed; runs on lx64 with TIERGATE_RUN_L8=1 KEVY_BIN=…"
fi
line "L10 rewrite-cold (B10)"  "PENDING(T4)" "BGREWRITEAOF on mostly-cold: digest equal + RAM bounded"
line "L11 boot>budget (B11)"   "PENDING(T4)" "replay of dataset>budget: RSS <= budget x 1.05 throughout"
env_line L12 "L12 D1-envelope"         "10M x 1KiB on 3GB + 2 idx: C4/C5 hold, hydration p95<=10ms"
env_line L13 "L13 hydration-batch (D3)" "one batched submission per page; preads == rows"
env_line L14 "L14 mixed-isolation (D4)" "hot p99 unchanged under cold scan + backfill"

echo
if [ "$fail" -ne 0 ]; then
  echo "tiergate: RED — pending lines remain (expected until their trains land)"
  exit 1
fi
echo "tiergate: PASS"
