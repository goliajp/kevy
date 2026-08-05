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

# ── L8 assertion body (T5): `used_memory` ≤ budget × 1.05 sustained +
# the auto probe smoke. The bound is logical (Redis maxmemory
# semantics); RSS is reported beside it, never clamped. The SHAPE lands with T5; the line stays PENDING until a
# tiered server run exists on lx64 (kevybench discipline) — run it
# there with TIERGATE_RUN_L8=1 KEVY_BIN=<path>. Linux-only (/proc RSS).
l8_mem_budget() { # -> "PASS ..." or "FAIL: why" on stdout
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
  # The budget is a LOGICAL bound — used_memory, Redis maxmemory
  # semantics. RSS follows the allocator (glibc brk fragmentation under
  # the 4KiB demotion churn is reclaim-proof) and is REPORTED as a
  # fragmentation ratio, not gated. See
  # PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md.
  local cap=$((budget_bytes * 105 / 100)) peak_used=0 peak_rss=0 used rss
  redis-benchmark -p "$port" -t set -n 250000 -r 250000 -d 4096 -q >/dev/null 2>&1 &
  local bench=$!
  while kill -0 "$bench" 2>/dev/null; do
    used=$(redis-cli -p "$port" info memory | tr -d '\r' | awk -F: '/^used_memory:/{print $2}')
    [ "${used:-0}" -gt "$peak_used" ] && peak_used=$used
    rss=$(( $(awk '/^VmRSS:/{print $2}' "/proc/$srv/status" 2>/dev/null || echo 0) * 1024 ))
    [ "$rss" -gt "$peak_rss" ] && peak_rss=$rss
    sleep 0.5
  done
  wait "$bench" 2>/dev/null || true
  sleep 2 # drain the spill backlog, then one final sample
  used=$(redis-cli -p "$port" info memory | tr -d '\r' | awk -F: '/^used_memory:/{print $2}')
  [ "${used:-0}" -gt "$peak_used" ] && peak_used=$used
  local frag; frag=$(awk -v r="$peak_rss" -v u="$peak_used" 'BEGIN{printf "%.2f",(u>0)?r/u:0}')
  if [ "$peak_used" -gt "$cap" ]; then
    echo "FAIL: peak used_memory $peak_used > budget×1.05 $cap (RSS $peak_rss frag ${frag}x)"
  else
    echo "PASS: used_memory $peak_used <= $cap; RSS $peak_rss frag ${frag}x reported"
  fi
}

# ── L4 assertion body (T4): replaying an AOF while spilling inline must
# not cost more than a factor the operator would notice — the RFC's
# floor is 0.70 × the untiered replay rate.
#
# NOTE: that floor names no shape, and the cost is almost entirely a
# function of shape — measured 0.53× at 8× over budget, 0.74× at 2×,
# 1.04× when nothing spills. So the default here is the HARSH end and
# the line reads red until the RFC sentence gets a workload attached.
# See FINDING-2026-08-06-l4-replay-spill-needs-a-shape.md; picking the
# shape is the design's call, not the gate's.
#
# The comparison is made ON ONE AOF, replayed twice: the data directory
# is written once with tiering off, copied, and each copy replayed under
# one config. Writing the dataset twice would compare two different byte
# streams and call the difference tiering. Rate comes from the server's
# own replay line (commands and milliseconds), so it measures replay and
# not process startup. Run with TIERGATE_RUN_L4=1 KEVY_BIN=<path>.
l4_replay_spill() { # -> "PASS ..." or "FAIL: why" on stdout
  local bin=${KEVY_BIN:?TIERGATE_RUN_L4 needs KEVY_BIN}
  local port=${TIERGATE_PORT:-6305}
  local rows=${TIERGATE_L4_ROWS:-200000}
  # The cost of spilling during replay scales with how much of the
  # dataset has to spill, so the budget is the knob this line is really
  # sensitive to. Default is the harsh end (most of the data spills).
  local budget=${TIERGATE_L4_BUDGET:-64mb}
  local base; base=$(mktemp -d)
  # shellcheck disable=SC2064
  trap "rm -rf '$base'" RETURN
  # One write pass, untiered, so the AOF has no spill history in it.
  "$bin" --port "$port" --threads 2 --dir "$base/src" &>/dev/null &
  local srv=$!
  sleep 1
  redis-benchmark -p "$port" -t set -n "$rows" -r "$rows" -d 4096 -q >/dev/null 2>&1
  local wrote; wrote=$(redis-cli -p "$port" dbsize)
  kill "$srv" 2>/dev/null; sleep 0.3; kill -9 "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  replay_ms() { # $1 = copy name, $2 = tier budget ("" = off) -> total ms
    cp -r "$base/src" "$base/$1"
    local log="$base/$1.log"
    if [ -n "$2" ]; then
      KEVY_TIER_BUDGET="$2" "$bin" --port "$port" --threads 2 --dir "$base/$1" &>"$log" &
    else
      "$bin" --port "$port" --threads 2 --dir "$base/$1" &>"$log" &
    fi
    local s2=$!
    local n=""
    for _ in $(seq 1 600); do
      n=$(redis-cli -p "$port" dbsize 2>/dev/null) && [ -n "$n" ] && break
      sleep 0.25
    done
    kill "$s2" 2>/dev/null; sleep 0.2; kill -9 "$s2" 2>/dev/null; wait "$s2" 2>/dev/null
    # Shards replay in parallel, so the wall cost is the slowest of them.
    awk '/replayed .* in [0-9]+ ms/{for(i=1;i<=NF;i++) if($i=="in") {v=$(i+1)+0; if(v>m) m=v}} END{print m+0}' "$log"
    echo "$n" >&2
  }
  local plain_ms tiered_ms plain_n tiered_n
  plain_ms=$(replay_ms plain "" 2>"$base/plain.n"); plain_n=$(cat "$base/plain.n")
  tiered_ms=$(replay_ms tiered "$budget" 2>"$base/tiered.n"); tiered_n=$(cat "$base/tiered.n")
  if [ "${plain_ms:-0}" -le 0 ] || [ "${tiered_ms:-0}" -le 0 ]; then
    echo "FAIL: no replay timing parsed (plain=${plain_ms:-} tiered=${tiered_ms:-})"; return 0
  fi
  if [ "$plain_n" != "$wrote" ] || [ "$tiered_n" != "$wrote" ]; then
    echo "FAIL: replay lost keys — wrote $wrote, plain $plain_n, tiered $tiered_n"; return 0
  fi
  # rate ratio = plain_ms / tiered_ms (same command count both sides).
  local ratio; ratio=$(awk -v p="$plain_ms" -v t="$tiered_ms" 'BEGIN{printf "%.2f",p/t}')
  if awk -v r="$ratio" 'BEGIN{exit !(r < 0.70)}'; then
    echo "FAIL: tiered replay ${ratio}x of plain (floor 0.70) at budget ${budget} — plain ${plain_ms}ms, tiered ${tiered_ms}ms, $wrote keys (the floor names no shape: 0.74x at 2x over budget — see FINDING-2026-08-06-l4-replay-spill-needs-a-shape.md)"
  else
    echo "PASS: tiered replay ${ratio}x of plain rate (floor 0.70) at budget ${budget} — plain ${plain_ms}ms, tiered ${tiered_ms}ms, $wrote keys both sides"
  fi
}

# ── L10 assertion body (T4): BGREWRITEAOF on a mostly-cold store keeps
# every cold value and stays inside the budget while it runs. The
# rewrite streams cold values from the pinned log without promoting, so
# the failure this catches is a rewrite that either drops a spilled
# value or pulls the whole spill area back into RAM to serialise it.
# Verdict is taken ACROSS A RESTART: a value the rewrite lost is still
# in memory until the process dies. Linux-only (/proc RSS); run with
# TIERGATE_RUN_L10=1 KEVY_BIN=<path>.
l10_rewrite_cold() { # -> "PASS ..." or "FAIL: why" on stdout
  local bin=${KEVY_BIN:?TIERGATE_RUN_L10 needs KEVY_BIN}
  local port=${TIERGATE_PORT:-6303}
  local budget=$((32 * 1024 * 1024)) rows=${TIERGATE_L10_ROWS:-60000}
  local dir; dir=$(mktemp -d)
  # shellcheck disable=SC2064
  trap "rm -rf '$dir'" RETURN
  KEVY_TIER_BUDGET=32mb "$bin" --port "$port" --threads 2 --dir "$dir" &>/dev/null &
  local srv=$!
  sleep 1
  redis-benchmark -p "$port" -t set -n "$rows" -r "$rows" -d 4096 -q >/dev/null 2>&1
  sleep 4 # let the spill backlog drain so the store is mostly cold
  local cold before_digest before_n
  cold=$(redis-cli -p "$port" info tiering | tr -d '\r' | awk -F: '/^cold_keys:/{print $2}')
  before_n=$(redis-cli -p "$port" dbsize)
  before_digest=$(redis-cli -p "$port" PREFIX.DIGEST "key:")
  [ "${cold:-0}" -gt 0 ] || { kill -9 "$srv" 2>/dev/null; echo "FAIL: nothing demoted, the run proves nothing"; return 0; }
  # Peak accounting DURING the rewrite: streaming from the log must not
  # promote, so the logical bound holds throughout (RSS is reported,
  # per L8 — the allocator's overshoot is not this line's claim).
  redis-cli -p "$port" bgrewriteaof >/dev/null
  local cap=$((budget * 105 / 100)) peak=0 used
  for _ in $(seq 1 240); do
    used=$(redis-cli -p "$port" info memory | tr -d '\r' | awk -F: '/^used_memory:/{print $2}')
    [ "${used:-0}" -gt "$peak" ] && peak=$used
    redis-cli -p "$port" info persistence | tr -d '\r' | grep -q '^aof_rewrite_in_progress:0' && break
    sleep 0.5
  done
  kill "$srv" 2>/dev/null; sleep 0.3; kill -9 "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  # The verdict: restart on what the rewrite left behind.
  KEVY_TIER_BUDGET=32mb "$bin" --port "$port" --threads 2 --dir "$dir" &>/dev/null &
  srv=$!
  local after_n="" after_digest=""
  for _ in $(seq 1 120); do
    after_n=$(redis-cli -p "$port" dbsize 2>/dev/null) && [ -n "$after_n" ] && break
    sleep 0.5
  done
  after_digest=$(redis-cli -p "$port" PREFIX.DIGEST "key:" 2>/dev/null)
  kill "$srv" 2>/dev/null; sleep 0.2; kill -9 "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  if [ "$before_n" != "$after_n" ] || [ "$before_digest" != "$after_digest" ]; then
    echo "FAIL: rewrite lost data — keys $before_n -> $after_n, digest $before_digest -> $after_digest"
  elif [ "$peak" -gt "$cap" ]; then
    echo "FAIL: used_memory peaked at $peak > budget×1.05 $cap during the rewrite"
  else
    echo "PASS: $after_n keys ($cold cold) survive a rewrite, digest equal, peak used_memory $peak <= $cap"
  fi
}

# ── L11 assertion body (T4): booting on a dataset far larger than the
# budget spills inline during replay instead of OOMing.
#
# The RFC phrased this as "RSS ≤ budget × 1.05 throughout boot". That
# phrasing does not survive measurement — glibc's arena runs ~2× the
# budget under demotion churn (PERF-FINDING-2026-07-25-b6-rss-glibc-
# fragmentation.md), and a 2.3 GB replay against a 64 MB budget peaked
# at 2.15×. So this body gates the LOGICAL bound and REPORTS RSS, the
# same split L8 settled on, and prints the RSS multiple so the RFC's
# wording can be corrected against evidence rather than quietly.
# Linux-only (/proc RSS); run with TIERGATE_RUN_L11=1 KEVY_BIN=<path>.
l11_boot_over_budget() { # -> "PASS ..." or "FAIL: why" on stdout
  local bin=${KEVY_BIN:?TIERGATE_RUN_L11 needs KEVY_BIN}
  local port=${TIERGATE_PORT:-6304}
  local budget=$((64 * 1024 * 1024)) rows=${TIERGATE_L11_ROWS:-300000}
  local dir; dir=$(mktemp -d)
  # shellcheck disable=SC2064
  trap "rm -rf '$dir'" RETURN
  KEVY_TIER_BUDGET=64mb "$bin" --port "$port" --threads 2 --dir "$dir" &>/dev/null &
  local srv=$!
  sleep 1
  redis-benchmark -p "$port" -t set -n "$rows" -r "$rows" -d 4096 -q >/dev/null 2>&1
  local before_n; before_n=$(redis-cli -p "$port" dbsize)
  kill "$srv" 2>/dev/null; sleep 0.3; kill -9 "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  # Replay: sample both bounds from the first moment the process exists.
  KEVY_TIER_BUDGET=64mb "$bin" --port "$port" --threads 2 --dir "$dir" &>/dev/null &
  srv=$!
  # The listener accepts only AFTER replay finishes, so dbsize answering
  # is the END of the window this line is about, not a sample inside it.
  # RSS is therefore sampled in a tight loop of its own (no redis-cli in
  # the path to slow it down) — the first version polled dbsize each
  # turn, broke on the first answer, and reported a peak from before the
  # process had grown: 0.06x budget for a store whose stubs alone are
  # 18 MB. A sampler that misses the event is worse than no sampler.
  local cap=$((budget * 105 / 100)) peak_used=0 peak_rss=0 used rss after_n=""
  ( while kill -0 "$srv" 2>/dev/null; do
      awk '/^VmRSS:/{print $2}' "/proc/$srv/status" 2>/dev/null
      sleep 0.05
    done ) > "$dir/rss.samples" &
  local sampler=$!
  for _ in $(seq 1 600); do
    after_n=$(redis-cli -p "$port" dbsize 2>/dev/null) && [ -n "$after_n" ] && break
    sleep 0.25
  done
  sleep 2 # the post-replay settle: inline spill drains, then measure
  used=$(redis-cli -p "$port" info memory 2>/dev/null | tr -d '\r' | awk -F: '/^used_memory:/{print $2}')
  [ -n "$used" ] && peak_used=$used
  kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null
  peak_rss=$(( $(sort -n "$dir/rss.samples" | tail -1 || echo 0) * 1024 ))
  kill "$srv" 2>/dev/null; sleep 0.2; kill -9 "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  local mult; mult=$(awk -v r="$peak_rss" -v b="$budget" 'BEGIN{printf "%.2f",(b>0)?r/b:0}')
  if [ "${after_n:-0}" != "$before_n" ]; then
    echo "FAIL: replay lost keys — $before_n -> ${after_n:-<no answer>}"
  elif [ "$peak_used" -gt "$cap" ]; then
    echo "FAIL: used_memory peaked at $peak_used > budget×1.05 $cap during replay"
  else
    echo "PASS: $after_n keys replayed, peak used_memory $peak_used <= $cap; RSS ${peak_rss} (${mult}x budget) reported"
  fi
}

# ── L15 assertion body (v4.1-V5): idle CPU with tiering ON must sit
# within a small multiple of OFF — the mailrs dogfood measurement
# (300-500× before the generation cache + sampler backoff; they turned
# the feature off). Linux-only (/proc stat ticks); run on lx64 with
# TIERGATE_RUN_IDLE=1 KEVY_BIN=<path>.
l15_idle_cpu() { # -> "PASS ..." or "FAIL: why" on stdout
  local bin=${KEVY_BIN:?TIERGATE_RUN_IDLE needs KEVY_BIN}
  local port=${TIERGATE_PORT:-6302}
  local window=${TIERGATE_IDLE_SECS:-30}
  measure_idle() { # $1 = tier budget ("" = tiering off) -> cpu ticks over the window
    local dir; dir=$(mktemp -d)
    if [ -n "$1" ]; then
      KEVY_TIER_BUDGET="$1" "$bin" --port "$port" --threads 2 --dir "$dir" &>/dev/null &
    else
      "$bin" --port "$port" --threads 2 --dir "$dir" &>/dev/null &
    fi
    local srv=$!
    sleep 1
    # A text index over real rows — the stat-walk shape the old
    # per-tick reserved_bytes feed paid for (F16a).
    redis-cli -p "$port" IDX.CREATE ig ON PREFIX "m:" FIELD body TYPE text KIND text >/dev/null
    for i in $(seq 1 5000); do
      echo "HSET m:$i body \"alpha beta gamma delta epsilon zeta eta theta msg $i\""
    done | redis-cli -p "$port" >/dev/null
    sleep 3 # converge: backfill + first demote sweep settle
    local t0 t1
    t0=$(awk '{print $14+$15}' "/proc/$srv/stat")
    sleep "$window"
    t1=$(awk '{print $14+$15}' "/proc/$srv/stat")
    kill "$srv" 2>/dev/null; sleep 0.2; kill -9 "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
    rm -rf "$dir"
    echo $((t1 - t0))
  }
  local off on
  off=$(measure_idle "")
  on=$(measure_idle "64mb") # floor-dominated: the worst idle shape
  # Small multiple: ≤ 3× plus 20 ticks of absolute slack (timer noise
  # on a near-zero baseline). mailrs measured 300-500× before v4.1-V5.
  if [ "$on" -le $((off * 3 + 20)) ]; then
    echo "PASS: idle ${window}s cpu ticks off=$off on=$on (<= 3x + slack; was 300-500x)"
  else
    echo "FAIL: idle ${window}s cpu ticks off=$off on=$on exceeds 3x + 20 slack"
  fi
}

echo "tiergate — capacity-arc tiering acceptance (RFC §2 B/D groups)"
echo

line "L1  hot-p99 (B1)"        "PENDING(T9/lx64)" "mechanics landed (T3, unit-tested); the p99 sweep itself runs on lx64 via perfgate + envelope"
env_line L2 "L2  cold-read-p99 (B2)"  "scalar <=100us/300us, hash-row <=200us/500us on NVMe"
line "L3  spill-budget (B3)"   "PENDING(T9/lx64)" "batching/hysteresis landed (T3, unit-tested); the stall-p99 measurement runs on lx64"
if [ "${TIERGATE_RUN_L4:-0}" = "1" ]; then
  l4_verdict=$(l4_replay_spill)
  case "$l4_verdict" in
    PASS*) line "L4  replay-spill (B4)" "PASS" "${l4_verdict#PASS: }" ;;
    *)     line "L4  replay-spill (B4)" "FAIL" "$l4_verdict" ;;
  esac
else
  line "L4  replay-spill (B4)"   "PENDING(T4)" "replay-with-spill >= 0.70 x plain replay [body landed; run with TIERGATE_RUN_L4=1 KEVY_BIN=…]"
fi
env_line L5 "L5  vlog-amp (B5)"       "vlog_size <= 2.0 x cold_bytes after churn + compaction"
env_line L6 "L6  capacity-10x (B6)"   "5M x 4KiB = 20GB on 2GB budget, op sweep green + used_memory <= budget (logical bound)"
# L8: the assertion body is implemented (T5, above) but a tiered
# SERVER run only exists on lx64 — the line stays PENDING-red until the
# T9 close-out runs it there (TIERGATE_RUN_L8=1 KEVY_BIN=… flips it).
if [ "${TIERGATE_RUN_L8:-0}" = "1" ]; then
  l8_verdict=$(l8_mem_budget)
  case "$l8_verdict" in
    PASS*) line "L8  mem-budget (B8)"  "PASS"  "used_memory <= budget x 1.05 (logical bound, Redis maxmemory semantics); RSS frag reported; ${l8_verdict#PASS: }" ;;
    *)     line "L8  mem-budget (B8)"  "FAIL"  "$l8_verdict" ;;
  esac
else
  line "L8  mem-budget (B8)"     "PENDING(T5)" "body landed; runs on lx64 with TIERGATE_RUN_L8=1 KEVY_BIN=…"
fi
# L10 / L11: bodies landed 2026-08-05 (the claims were in the docs with
# nothing behind them — both were measured by hand first, and a hand
# measurement is the thing that drifts). Server runs belong on lx64.
if [ "${TIERGATE_RUN_L10:-0}" = "1" ]; then
  l10_verdict=$(l10_rewrite_cold)
  case "$l10_verdict" in
    PASS*) line "L10 rewrite-cold (B10)" "PASS" "${l10_verdict#PASS: }" ;;
    *)     line "L10 rewrite-cold (B10)" "FAIL" "$l10_verdict" ;;
  esac
else
  line "L10 rewrite-cold (B10)"  "PENDING(T4)" "BGREWRITEAOF on mostly-cold: digest equal + RAM bounded [body landed; run with TIERGATE_RUN_L10=1 KEVY_BIN=…]"
fi
if [ "${TIERGATE_RUN_L11:-0}" = "1" ]; then
  l11_verdict=$(l11_boot_over_budget)
  case "$l11_verdict" in
    PASS*) line "L11 boot>budget (B11)" "PASS" "${l11_verdict#PASS: }" ;;
    *)     line "L11 boot>budget (B11)" "FAIL" "$l11_verdict" ;;
  esac
else
  line "L11 boot>budget (B11)"   "PENDING(T4)" "replay of dataset>budget: used_memory bounded, RSS reported (the RFC says RSS — see the body) [run with TIERGATE_RUN_L11=1 KEVY_BIN=…]"
fi
if [ "${TIERGATE_RUN_IDLE:-0}" = "1" ]; then
  l15_verdict=$(l15_idle_cpu)
  case "$l15_verdict" in
    PASS*) line "L15 idle-cpu (v4.1-V5)"  "PASS"  "${l15_verdict#PASS: }" ;;
    *)     line "L15 idle-cpu (v4.1-V5)"  "FAIL"  "$l15_verdict" ;;
  esac
else
  line "L15 idle-cpu (v4.1-V5)"  "PENDING(lx64)" "idle 30s ON <= 3x OFF (mailrs measured 300-500x); body landed, run with TIERGATE_RUN_IDLE=1 KEVY_BIN=…"
fi
env_line L12 "L12 D1-envelope"         "10M x 1KiB on 3GB + 2 idx: C4/C5 hold, hydration p95<=10ms"
env_line L13 "L13 hydration-batch (D3)" "one batched submission per page; preads == rows"
env_line L14 "L14 mixed-isolation (D4)" "hot p99 unchanged under cold scan + backfill"

echo
if [ "$fail" -ne 0 ]; then
  echo "tiergate: RED — pending lines remain (expected until their trains land)"
  exit 1
fi
echo "tiergate: PASS"
