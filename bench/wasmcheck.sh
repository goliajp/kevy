#!/usr/bin/env bash
# wasmcheck — the wasm-eligible crates compile for each wasm target.
#
#   bash bench/wasmcheck.sh                   # every target below
#   bash bench/wasmcheck.sh wasm32-wasip1     # one (CI's matrix passes one)
#
# CI and the suite both run this file, so the crate list lives here once.
# A target that is not installed is a refusal, not a pass.
set -uo pipefail
cd "$(dirname "$0")/.."

TARGETS=("$@")
[ ${#TARGETS[@]} -gt 0 ] || TARGETS=(wasm32-unknown-unknown wasm32-wasip1)
CRATES=(kevy-bytes kevy-hash kevy-map kevy-store kevy-persist kevy-resp kevy-verbs kevy-embedded kevy-wasm)

for t in "${TARGETS[@]}"; do
  rustup target list --installed 2>/dev/null | grep -qx "$t" || {
    echo "wasmcheck: REFUSED — $t is not installed (rustup target add $t)" >&2
    exit 2
  }
done
fail=0
for t in "${TARGETS[@]}"; do
  for c in "${CRATES[@]}"; do
    echo "  cargo check --target $t -p $c"
    cargo check -q --target "$t" -p "$c" || fail=1
  done
done
[ $fail -eq 0 ] || { echo "wasmcheck: FAIL — a crate above does not compile for wasm"; exit 1; }
echo "wasmcheck: PASS — ${#CRATES[@]} crates on ${#TARGETS[@]} target(s)"
