#!/usr/bin/env bash
# compressgate — the v5 memory-experiment gate for kevy-compress.
#
# RFC: .claude/rfcs/2026-07-26-v5-kevy-compress.md §6 (K1..K7)
# Accounting contract: bench/V5-ACCOUNTING-CONTRACT.md §2
#
# One line per acceptance criterion; a line is either a real assertion or
# PENDING(<train>). RED until every line it owns is green — the
# assertions exist before the crate, so the crate is built against them.
#
# This gate belongs to an EXPERIMENT. K4 is the line that says whether
# the design was worth doing, and §2 of the RFC — the corpus-versus-datum
# argument the whole dictionary case rests on — is the premise most worth
# killing early. If K4 cannot be made to pass, that is the finding, and
# it retires the train rather than triggering another round of tuning.
#
# Line ownership:
#   T3 (stone, unwired): K2 never-expands, K3 round-trip + rejection,
#                        K4 cross-value redundancy, and the K5 identity
#   T4 (wired):          K1 cold-read budget, K5 at envelope scale,
#                        K6 no encode on the SET path, K7 disposability
set -euo pipefail

fail=0
# COMPRESSGATE_UNIT_ONLY=1 narrows this run to the lines a checkout can
# assert by itself. K1 and K5-amplification are SLA numbers read from a
# full-scale lx64 envelope run; a GitHub runner has no such artifact and
# cannot produce one, so without this the gate is red everywhere except
# the box — and a gate that is red by default protects nothing, because
# nobody reads it.
#
# It narrows OWNERSHIP, never the verdict: a unit-fed line that FAILs
# still fails the run, and the envelope-fed lines are reported as not
# owned rather than quietly counted as passing. An unmeasured SLA line
# printed as PASS is the failure this project has already paid for once,
# when a probe died and its empty medians rendered as four bars over the
# limit.
UNIT_ONLY=${COMPRESSGATE_UNIT_ONLY:-0}
unowned=0
missing=0
regressed=0

line() { # name, status, detail
  local name="$1" status="$2" detail="$3"
  case "$UNIT_ONLY:$status" in
    1:PENDING*)
      printf '%-26s %-14s %s\n' "$name" "NOT-OWNED" "$detail"
      unowned=$((unowned + 1))
      return;;
  esac
  printf '%-26s %-14s %s\n' "$name" "$status" "$detail"
  if [ "$status" != "PASS" ]; then
    fail=1
    # Separated so the verdict can name what actually went wrong. A
    # regression and a missing artifact are different events, and a
    # verdict that blames the artifact for a regression sends the reader
    # to the box to run a bench when a codec just broke.
    case "$status" in
      PENDING*) missing=$((missing + 1));;
      *)        regressed=$((regressed + 1));;
    esac
  fi
}

echo "compressgate — kevy-compress acceptance (RFC 2026-07-26-v5-kevy-compress §6)"
echo "contract: bench/V5-ACCOUNTING-CONTRACT.md §2"
echo

ROOT=$(cd "$(dirname "$0")/.." && pwd)
if [ ! -d "$ROOT/crates/kevy-compress" ]; then
  echo "crates/kevy-compress does not exist yet — every line is PENDING(T3)."
  echo
fi

# ── K1: the criterion that can reject the design outright. At spg
# lzss's ~100 MiB/s a 4 KB value costs ~40 us against a 105 us budget,
# so memcpy-class decode is a design requirement, not an optimisation.
# K1/K5-amp consume the capacity-envelope results file (the tiergate
# pattern): SLA numbers count only from a full-scale lx64 run.
ENV_RESULTS=${COMPRESSGATE_ENVELOPE_RESULTS:-$ROOT/bench/.capacity-envelope-results}
env_line() { # $1 = line key -> the whole results line, or ""
  [ -f "$ENV_RESULTS" ] && grep -q "^SCALE=full" "$ENV_RESULTS" \
    && grep "^$1=" "$ENV_RESULTS" || true
}
k1_check() {
  local l; l=$(env_line L2)
  if [ -z "$l" ]; then echo "PENDING(T4) no full-scale envelope results"; return; fi
  case "$l" in L2=PASS*) echo "PASS $l";; *) echo "FAIL $l";; esac
}
k1_out=$(k1_check)
line "K1-cold-read-budget" "${k1_out%% *}" \
  "B2 cold-read p99 inside budget with compression ON — ${k1_out#* }"

# ── K2/K3: the two that make a codec safe to put under a data engine.
# A named test that no longer exists is not a passing test. `cargo test
# <filter>` prints "test result: ok. 0 passed" when the filter matches
# nothing, so grepping for "ok" alone goes green on a test somebody
# renamed away — the same shape as a criterion an empty data directory
# satisfies. Require a non-zero pass count, which a vanished test cannot
# produce.
ran_and_passed() { # $1 = crate, $2 = test filter
  local out
  out=$(cd "$ROOT" && cargo test -p "$1" "$2" 2>&1) || return 1
  echo "$out" | grep -qE "test result: ok\. [1-9][0-9]* passed"
}

t3_test() { # $1 = test filter -> "PASS ..." | "FAIL ..."
  if ran_and_passed kevy-compress "$1"; then
    echo "PASS"
  else
    echo "FAIL"
  fi
}
line "K2-never-expands" "$(t3_test k2_incompressible_never_expands)" \
  "encoded <= raw + frame header; adversarial input falls back to raw [unit; fuzz target roundtrip.rs]"
line "K3-roundtrip" "$(t3_test k3_corrupt_frames_reject)" \
  "round-trip identity; truncated/corrupt frames REJECTED [unit; fuzz targets roundtrip/decode_arbitrary]"

# ── K4: the structural claim. A per-datum baseline provably cannot pass
# this, which is the whole point — it is not a ratio, it is a category.
line "K4-cross-value" "$(t3_test k4_identical_values_collapse_against_the_dictionary)" \
  "1000 identical 400B values -> <=16 B/value against a shared dictionary; per-datum baseline pays ~half the value each"

# ── K5: identity first (contract §2), then the envelope number.
k5_identity() {
  if ran_and_passed kevy-vlog rotation_trains_a_dictionary; then
    echo "PASS"
  else
    echo "FAIL"
  fi
}
line "K5-identity" "$(k5_identity)" \
  "vlog stats.bytes == sum(header + frame body) EXACT, dictionary engaged across rotation [kevy-vlog unit]"
k5amp_check() {
  local l; l=$(env_line L5)
  if [ -z "$l" ]; then echo "PENDING(T4) no full-scale envelope results"; return; fi
  case "$l" in L5=PASS*) echo "PASS $l (uncompressed baseline was 1.27x)";; *) echo "FAIL $l";; esac
}
k5a_out=$(k5amp_check)
line "K5-amplification" "${k5a_out%% *}" \
  "vlog amplification on the (cross-value redundant) envelope corpus — ${k5a_out#* }"

# ── K6: provable by inspection, which is the point. Nothing on the hot
# path may call the encoder — that is what makes the KV/pubsub
# non-regression obligation a source property rather than a benchmark
# result. The source half can run the moment the crate exists.
k6_source() {
  if [ ! -d "$ROOT/crates/kevy-compress" ]; then
    echo "PENDING(T3) crate does not exist yet"
    return
  fi
  local hits
  hits=$(cd "$ROOT" && grep -rn "kevy_compress::.*encode\|compress::encode" \
    --include='*.rs' crates/kevy-store/src crates/kevy-rt/src 2>/dev/null \
    | grep -v -E 'demote|spill|compact' || true)
  if [ -z "$hits" ]; then
    echo "PASS no encode call outside the demote/compact paths"
  else
    echo "FAIL encode reachable from a non-demote path: $(echo "$hits" | head -3 | tr '\n' ' ')"
  fi
}
k6_out=$(k6_source)
line "K6-no-encode-on-set" "${k6_out%% *}" "${k6_out#* } [+ perfgate KV/pubsub lines at T4]"

# ── K7: the vlog is disposable by design (AOF is the only durability
# truth), and a dictionary that lives and dies with its file inherits
# that — which is what removes the format-compatibility burden entirely.
k7_check() {
  # The dictionary is a VlogFile struct field — it is never serialized
  # anywhere (structural: kevy-vlog writes only record bytes), and the
  # vlog disposability contract test still passes with frames in the
  # bodies. The B10/B11 envelope halves live with tiergate L10/L11.
  if ran_and_passed kevy-vlog open_is_disposable; then
    echo "PASS"
  else
    echo "FAIL"
  fi
}
line "K7-disposability" "$(k7_check)" \
  "dictionary lives only in VlogFile (never serialized); vlog disposability contract test green [B10/B11 at tiergate]"

echo
if [ "$fail" -ne 0 ]; then
  if [ "$regressed" -gt 0 ]; then
    echo "compressgate: RED — $regressed line(s) FAILED. That is a regression in"
    echo "  the codec or in the tests that back the criterion, not a missing"
    echo "  artifact; running a bench on the box will not turn it green."
  fi
  if [ "$missing" -gt 0 ]; then
    echo "compressgate: RED — $missing SLA line(s) have no measurement."
    echo "  K1 and K5-amplification read bench/.capacity-envelope-results from a"
    echo "  full-scale lx64 run beside this checkout. To assert only what a"
    echo "  checkout can assert, run with COMPRESSGATE_UNIT_ONLY=1."
  fi
  exit 1
fi
if [ "$UNIT_ONLY" = 1 ] && [ "$unowned" -gt 0 ]; then
  echo "compressgate(unit): PASS — $unowned SLA line(s) NOT OWNED by this run."
  echo "  They need bench/.capacity-envelope-results from a full-scale lx64 run."
  echo "  Not measured here, and not claimed here."
  exit 0
fi
echo "compressgate: PASS"
