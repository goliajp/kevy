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
# NOT IN CI, and that is not an oversight. The T1/T2 lines are green on
# the `r1-locality` branch, not on develop, so on develop this gate is
# red by construction — and a gate that is red by default teaches people
# to ignore it, which costs more than not running it. It goes into CI
# when that branch merges (the owner's call), at which point every
# PENDING(T2) line below has a real assertion behind it.
#
# Its sibling compressgate DOES run in CI, narrowed with
# COMPRESSGATE_UNIT_ONLY=1 to the lines a checkout can assert. The same
# split would work here once the crate is on the branch CI builds.
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

# ── M1/M2: the A/B. Two binaries built from the same source, one with
# the allocator feature and one without. The gate refuses to guess which
# is which, and refuses to invent a verdict without both.
BIN_OFF=${ALLOCGATE_BIN_OFF:-}
BIN_ON=${ALLOCGATE_BIN_ON:-}
AB_TOLERANCE=${ALLOCGATE_TOLERANCE:-0.92}
AB_ROUNDS=${ALLOCGATE_ROUNDS:-4}
AB_PORT=${ALLOCGATE_PORT:-7311}

ab_missing() { [ -z "$BIN_OFF" ] || [ -z "$BIN_ON" ] || [ ! -x "$BIN_OFF" ] || [ ! -x "$BIN_ON" ]; }

# ── M1 delegates to perfgate rather than re-implementing a measurement
# loop. perfgate already interleaves the two binaries and flips their
# order every instance, because this box slows monotonically across a
# long run and a fixed order silently favours whichever went first —
# that confound once manufactured an entire regression that did not
# exist. Reading its steady-state numbers off the server's own counters
# rather than the benchmark client's reported rate is the same
# discipline. Copying 500 lines to get all that again would only give it
# somewhere new to rot.
m1() {
  if ab_missing; then
    echo "PENDING(T2) needs ALLOCGATE_BIN_OFF + ALLOCGATE_BIN_ON (same commit, feature off/on), on lx64"
    return
  fi
  local out rc why
  # perfgate takes the candidate as a positional argument and the
  # reference through PERFGATE_REF_BIN.
  out=$(PERFGATE_REF_BIN="$BIN_OFF" bash "$ROOT/bench/perfgate.sh" "$BIN_ON" 2>&1) && rc=0 || rc=$?
  # Pass perfgate's own words through. A gate that swallows the reason
  # and prints "did not pass" makes the next person re-run it by hand to
  # learn anything.
  why=$(printf '%s' "$out" | grep -m1 -E 'perfgate: (FAIL|REFUSED)' \
        || printf '%s' "$out" | tail -1)
  case "$rc" in
    0) echo "PASS KV lines within tolerance with the allocator ON" ;;
    # perfgate exits 2 when it refuses to measure — a missing tool or a
    # dirty box. That is this machine failing to be a measuring
    # instrument, not the allocator losing, and calling it FAIL would
    # teach the next reader to ignore a red line. It still may not PASS:
    # an unmeasured angle stops the gate.
    2) echo "PENDING(T2) ${why:-perfgate refused}" ;;
    *) echo "FAIL ${why:-perfgate exited $rc with no output}" ;;
  esac
}

# ── M2: the same interleaving for pub/sub, which perfgate does not
# cover. Order flips every round for the same reason.
PUBSUB_BIN=${ALLOCGATE_PUBSUB_BIN:-$ROOT/target/release/kevy-pubsub-bench}

pubsub_once() { # $1 = server binary -> delivered msg/s, or empty
  local bin=$1 dir srv rate
  dir=$(mktemp -d "${TMPDIR:-/tmp}/allocgate-XXXXXX")
  KEVY_BIND=127.0.0.1 "$bin" --port "$AB_PORT" --dir "$dir" --threads 1 \
    >"$dir/server.log" 2>&1 &
  srv=$!
  # Ready when it answers, not after a fixed sleep: a slow cold start
  # would otherwise be measured as slow serving.
  local cli
  cli=${ALLOCGATE_CLI:-$(command -v redis-cli || echo "$ROOT/target/release/kevy-cli")}
  for _ in $(seq 1 50); do
    "$cli" -p "$AB_PORT" PING >/dev/null 2>&1 && break
    sleep 0.2
  done
  rate=$("$PUBSUB_BIN" --port "$AB_PORT" --subs 50 --msgs 20000 --size 64 2>/dev/null \
    | sed -n 's/.*delivered=\([0-9]*\) msg\/s.*/\1/p')
  kill "$srv" 2>/dev/null
  wait "$srv" 2>/dev/null
  rm -rf "$dir"
  printf '%s' "$rate"
}

m2() {
  if ab_missing; then
    echo "PENDING(T2) needs ALLOCGATE_BIN_OFF + ALLOCGATE_BIN_ON (same commit, feature off/on), on lx64"
    return
  fi
  [ -x "$PUBSUB_BIN" ] || { echo "PENDING(T2) kevy-pubsub-bench not built (cargo build --release -p kevy-pubsub-bench)"; return; }
  local offs="" ons="" i r
  for i in $(seq 1 "$AB_ROUNDS"); do
    if [ $((i % 2)) -eq 1 ]; then
      r=$(pubsub_once "$BIN_OFF"); offs="$offs $r"
      r=$(pubsub_once "$BIN_ON");  ons="$ons $r"
    else
      r=$(pubsub_once "$BIN_ON");  ons="$ons $r"
      r=$(pubsub_once "$BIN_OFF"); offs="$offs $r"
    fi
  done
  local med_off med_on
  med_off=$(printf '%s\n' $offs | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
  med_on=$(printf '%s\n' $ons | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
  if [ -z "$med_off" ] || [ -z "$med_on" ] || [ "$med_off" -eq 0 ] 2>/dev/null; then
    echo "FAIL pubsub produced no rate — an unmeasured angle must stop the gate, never pass quietly"
    return
  fi
  # Print every sample, not just the medians. A single ratio cannot be
  # told apart from noise, and reporting one as if it could is the
  # anti-pattern the perf methodology names outright — a gap smaller
  # than the baseline's own spread is not a gap.
  local spread
  # The ternary must be parenthesized: a bare `>` inside a printf
  # argument list is OUTPUT REDIRECTION to awk, which wrote the spread
  # into a file named after the computed value (a stray `4.72381` in
  # the checkout root) and handed stdout the empty string.
  spread=$(printf '%s\n' $offs | awk '{s+=$1; a[NR]=$1} END {
      m=s/NR; for(i=1;i<=NR;i++) v+=(a[i]-m)^2;
      printf "%.1f%%", (NR>1 ? 100*sqrt(v/(NR-1))/m : 0) }')
  awk -v on="$med_on" -v off="$med_off" -v tol="$AB_TOLERANCE" \
      -v sam_on="$ons" -v sam_off="$offs" -v sd="$spread" \
    'BEGIN { r = on / off;
             printf "%s allocator ON %.2fM vs OFF %.2fM msg/s (ratio %.3f, floor %.2f; OFF spread %s) ON=[%s] OFF=[%s]\n",
                    (r >= tol ? "PASS" : "FAIL"), on/1e6, off/1e6, r, tol, sd, sam_on, sam_off }'
}

m1_out=$(m1)
line "M1-kv-ab" "${m1_out%% *}" \
  "GET/SET/pipeline with the allocator ON, not merely OFF — ${m1_out#* }"
m2_out=$(m2)
line "M2-pubsub-ab" "${m2_out%% *}" "${m2_out#* }"

# ── T1 lines run the crate's own tests. A gate that only ever reports
# PENDING teaches nothing; once the assertions exist, it runs them.
run_t1() { # $1 = test name filter -> "PASS ..." / "FAIL ..." / "SKIP ..."
  if [ ! -d "$ROOT/crates/kevy-alloc" ]; then
    echo "SKIP crate does not exist yet"
    return
  fi
  local out n
  if out=$(cd "$ROOT" && cargo test -p kevy-alloc --features global --lib "$1" 2>&1); then
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
# The integration test installs KevyAlloc as this test binary's own
# allocator, so the standard library is the caller — which is how the
# cross-thread accounting defect was found in the first place.
run_m5() {
  if [ ! -d "$ROOT/crates/kevy-alloc" ]; then
    echo "SKIP crate does not exist yet"
    return
  fi
  local out n
  if out=$(cd "$ROOT" && cargo test -p kevy-alloc --features global --test global_alloc 2>&1); then
    n=$(printf '%s' "$out" | sed -n 's/.*test result: ok\. \([0-9][0-9]*\) passed.*/\1/p' | head -1)
    if [ "${n:-0}" -eq 0 ]; then
      echo "FAIL the global-allocator suite asserted nothing"
    else
      echo "PASS $n assertion(s) green under KevyAlloc as the process allocator"
    fi
  else
    echo "FAIL $(printf '%s' "$out" | grep -m1 -E 'panicked at|allocation of' || echo 'the global-allocator suite failed')"
  fi
}
m5_out=$(run_m5)
line "M5-foreign-free" "${m5_out%% *}" \
  "cross-thread frees + threads exiting while their memory is held — ${m5_out#* }"

# ── M6: torajs c2970b6d's tuition — an uncapped span pool is a SIGSEGV,
# not a leak (alloc returned None, the null propagated into a write).
m6_out=$(run_t1 m6_)
line "M6-class-cap" "${m6_out%% *}" \
  "PER_CLASS_CAP honoured; exhaustion answers None, never a wild pointer — ${m6_out#* }"

# ── M7: the allocator is under everything, so everything must still pass.
line "M7-existing-gates" "PENDING(T2)" \
  "crashgate/availgate/tiergate/tablegate/textgate/oracle green with KEVY_BIN=\$ALLOCGATE_BIN_ON [runner: each gate on lx64 against the allocator-on build]"

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
