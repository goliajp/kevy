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
line() { # name, status, detail
  local name="$1" status="$2" detail="$3"
  printf '%-26s %-14s %s\n' "$name" "$status" "$detail"
  if [ "$status" != "PASS" ]; then fail=1; fi
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
line "K1-cold-read-budget" "PENDING(T4)" \
  "cold read p99 stays inside B2 (145us hash / 105us scalar) with compression ON [capacity-envelope]"

# ── K2/K3: the two that make a codec safe to put under a data engine.
line "K2-never-expands" "PENDING(T3)" \
  "encoded <= raw + frame header for EVERY input incl. adversarial [fuzz]"
line "K3-roundtrip" "PENDING(T3)" \
  "round-trip identity; truncated/corrupt frames REJECTED, never mis-decoded [fuzz]"

# ── K4: the structural claim. A per-datum baseline provably cannot pass
# this, which is the whole point — it is not a ratio, it is a category.
line "K4-cross-value" "PENDING(T3)" \
  "N identical 400B values in one segment -> O(dictionary) + N x small; a per-datum baseline fails it by construction"

# ── K5: identity first (contract §2), then the envelope number.
line "K5-identity" "PENDING(T3)" \
  "stored == dictionary + frame_overhead + payload, EXACT (no tolerance)"
line "K5-amplification" "PENDING(T4)" \
  "vlog amplification improves on B5's 1.27x for compressible corpora, and compact_below still terminates"

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
line "K7-disposability" "PENDING(T4)" \
  "no dictionary state outside a vlog file; AOF remains the sole durability truth [tier_persistence B10/B11]"

echo
if [ "$fail" -ne 0 ]; then
  echo "compressgate: RED — as designed at T0. Lines turn green as T3/T4 land."
  exit 1
fi
echo "compressgate: PASS"
