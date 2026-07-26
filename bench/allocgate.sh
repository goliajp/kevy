#!/usr/bin/env bash
# allocgate — the v5 memory-experiment gate for kevy-alloc.
#
# RFC: .claude/rfcs/2026-07-26-v5-kevy-alloc.md §8 (M1..M8)
# Accounting contract: bench/V5-ACCOUNTING-CONTRACT.md §1
#
# One line per acceptance criterion; a line is either a real assertion or
# PENDING(<train>). The gate is RED until every line it owns is green —
# red-first is the point (crashgate/tiergate precedent): the assertions
# exist before the crate, so the crate is built against them.
#
# This gate belongs to an EXPERIMENT. A line going red is a result: if a
# premise dies here, the premise changes (ROADMAP v5 rule 5) — nobody
# widens a tolerance to make a line pass.
#
# Line ownership:
#   T1 (stone, unwired): M4 reclaim, M6 per-class cap, M8 unsafe
#                        containment, and the M3 identity at unit scale
#   T2 (wired, lx64):    M1 KV A/B, M2 pubsub A/B, M3 at envelope scale,
#                        M5 foreign-free stress, M7 existing gates
#
# M1/M2 note — why the A/B lives here and not in perfgate:
#   perfgate is a ratchet over time against a recorded baseline. The
#   allocator question is a different one: same source, two builds
#   (feature off / on), same box, interleaved. That is an A/B at one
#   instant, so it lives here. perfgate gains allocator-ON baselines
#   only once the allocator is default-on (end of T2) — at which point
#   its existing lines already measure it, with no new metrics needed.
set -euo pipefail

fail=0
line() { # name, status, detail
  local name="$1" status="$2" detail="$3"
  printf '%-26s %-14s %s\n' "$name" "$status" "$detail"
  if [ "$status" != "PASS" ]; then fail=1; fi
}

echo "allocgate — kevy-alloc acceptance (RFC 2026-07-26-v5-kevy-alloc §8)"
echo "contract: bench/V5-ACCOUNTING-CONTRACT.md §1"
echo

# ── Does the crate even exist yet? Every line below is PENDING until it
# does; saying so once is clearer than repeating it eight times.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
if [ ! -d "$ROOT/crates/kevy-alloc" ]; then
  echo "crates/kevy-alloc does not exist yet — every line is PENDING(T1)."
  echo
fi

# ── M1/M2: the A/B. Needs two binaries built from the same source with
# the allocator feature off and on. Refuses to guess which is which.
ab_ready=0
if [ -n "${ALLOCGATE_BIN_OFF:-}" ] && [ -n "${ALLOCGATE_BIN_ON:-}" ]; then
  ab_ready=1
fi
ab_line() { # $1 = display name, $2 = what it will assert
  if [ "$ab_ready" = 1 ]; then
    line "$1" "PENDING(T2)" "$2 [binaries provided; assertion body lands with T2]"
  else
    line "$1" "PENDING(T2)" "$2 [needs ALLOCGATE_BIN_OFF + ALLOCGATE_BIN_ON, interleaved, on lx64]"
  fi
}

ab_line "M1-kv-ab" \
  "GET/SET/pipeline within perfgate tolerance with the allocator ON (not merely OFF)"
ab_line "M2-pubsub-ab" \
  "publish/deliver within tolerance with the allocator ON"

# ── T1 lines run the crate's own tests. A gate that only ever reports
# PENDING teaches nothing; once the assertions exist, it runs them.
run_t1() { # $1 = test name filter -> "PASS ..." / "FAIL ..." / "SKIP ..."
  if [ ! -d "$ROOT/crates/kevy-alloc" ]; then
    echo "SKIP crate does not exist yet"
    return
  fi
  local out n
  if out=$(cd "$ROOT" && cargo test -p kevy-alloc --lib "$1" 2>&1); then
    n=$(printf '%s' "$out" | sed -n 's/.*test result: ok\. \([0-9][0-9]*\) passed.*/\1/p' | head -1)
    if [ "${n:-0}" -eq 0 ]; then
      # A filter matching nothing is the failure mode that makes a gate
      # lie: green because it asserted nothing.
      echo "FAIL filter '$1' matched no test"
    else
      echo "PASS $n assertion(s) green"
    fi
  else
    echo "FAIL $(printf '%s' "$out" | grep -m1 'panicked at' || echo 'cargo test failed')"
  fi
}

# ── M3: the identity and the scaling claim (contract §1).
m3_out=$(run_t1 identity)
line "M3-identity" "${m3_out%% *}" \
  "mapped == live+rounding+cache+span_free+virgin+hysteresis+overhead, EXACT — ${m3_out#* }"
line "M3-scaling" "PENDING(T2)" \
  "across two dataset sizes only rounding may grow; slack/cache/hysteresis flat [capacity-envelope B6 + a 400B variant]"
line "M3-rss-residual" "PENDING(T2)" \
  "RSS - mapped reported by name and flat in dataset size (a growing residual = an allocation path we missed)"

# ── M4: the property the whole experiment rests on.
m4_out=$(run_t1 m4_)
line "M4-reclaim" "${m4_out%% *}" \
  "emptied spans give their pages back — ${m4_out#* } (2 on Linux, 1 elsewhere: the kernel-RSS half needs MADV_DONTNEED, and macOS MADV_FREE would make it meaningless)"

# ── M5: kevy genuinely frees across shards (Arc<Box<[u8]>> on the shared
# read lane), so torajs's documented-and-accepted ABA note may not be
# inherited — it has to be re-argued or designed out.
line "M5-foreign-free" "PENDING(T2)" \
  "N-core concurrent alloc/free stress: no lost slots, no double-hand-out, no ABA-by-inheritance"

# ── M6: torajs c2970b6d's tuition — an uncapped span pool is a SIGSEGV,
# not a leak (alloc returned None, the null propagated into a write).
m6_out=$(run_t1 m6_)
line "M6-class-cap" "${m6_out%% *}" \
  "PER_CLASS_CAP honoured; exhaustion answers None, never a wild pointer — ${m6_out#* }"

# ── M7: the allocator is under everything, so everything must still pass.
line "M7-existing-gates" "PENDING(T2)" \
  "crashgate/availgate/tiergate/tablegate/textgate/oracle all green with the allocator ON"

# ── M8: unsafe containment, as a ratchet on a recorded set.
#
# The RFC first claimed unsafe lived in "kevy-sys / kevy-uring /
# kevy-madvise"; recording the set at T0 showed fourteen crates, which
# is what an engine with FFI doors, a wasm ABI, a raw-entry map and a
# uring reactor actually looks like. The claim was wrong and the RFC is
# corrected. What is worth gating is not a small number but that the
# number does not quietly grow — kevy-alloc is a deliberate addition,
# pre-approved in the baseline; anything else is not.
#
# Runs today: a source property, no build needed.
UNSAFE_BASELINE=${ALLOCGATE_UNSAFE_BASELINE:-$ROOT/bench/.unsafe-crates-baseline}
m8() {
  [ -f "$UNSAFE_BASELINE" ] || { echo "FAIL baseline missing: $UNSAFE_BASELINE"; return; }
  local allowed actual extra
  allowed=$(grep -v '^#' "$UNSAFE_BASELINE" | grep -v '^[[:space:]]*$' | sort)
  actual=$(cd "$ROOT" && for d in crates/*/; do
    local c n
    c=$(basename "$d")
    n=$(grep -rlE '(^|[^_[:alnum:]])unsafe[[:space:]]*(\{|fn |impl |extern |trait )' \
        --include='*.rs' "$d/src" 2>/dev/null \
        | grep -v -E '/(tests?|abi_tests|[a-z_]*_tests)\.rs$' | wc -l | tr -d ' ')
    [ "$n" != 0 ] && echo "$c"
  done | sort)
  extra=$(comm -13 <(echo "$allowed") <(echo "$actual"))
  if [ -z "$extra" ]; then
    echo "PASS $(echo "$actual" | wc -l | tr -d ' ') crates carry unsafe, none outside the recorded set"
  else
    echo "FAIL unsafe appeared in a crate outside the baseline: $(echo "$extra" | tr '\n' ' ')"
  fi
}
m8_out=$(m8)
line "M8-unsafe-ratchet" "${m8_out%% *}" "${m8_out#* } [baseline: bench/.unsafe-crates-baseline]"

echo
if [ "$fail" -ne 0 ]; then
  echo "allocgate: RED — as designed at T0. Lines turn green as T1/T2 land."
  exit 1
fi
echo "allocgate: PASS"
