#!/usr/bin/env bash
# Clippy the way CI does — every target in its matrix, not just this machine's.
#
#   bash bench/clippygate.sh
#
# A `#[cfg(target_arch = ...)]` branch is invisible to a local lint run: on an
# M-series Mac the x86 half of crates/kevy-sys/src/checksum.rs is never
# compiled, so a lint that fires there passes locally and fails on CI. That is
# not hypothetical — it cost a red develop on 2026-08-23, on a file the same
# session had just swept for exactly that lint on the arch it could see.
#
# CI's matrix lives in .github/workflows/ci.yml and is read from there rather
# than restated, so adding a target to CI cannot leave this gate behind.
set -uo pipefail
cd "$(dirname "$0")/.."

CI=.github/workflows/ci.yml
[ -f "$CI" ] || { echo "clippygate: REFUSED — no $CI to read the matrix from" >&2; exit 2; }

# The `test` job's matrix entries: lines of the form `- target: <triple>`
# before the wasm job's differently-shaped list.
TARGETS=$(awk '/^      matrix:/{n++} n==1 && /- target:/{print $3}' "$CI")
[ -n "$TARGETS" ] || {
  echo "clippygate: REFUSED — parsed no targets out of $CI; the matrix shape changed" >&2
  exit 2
}
echo "clippygate: CI targets = $(echo "$TARGETS" | tr '\n' ' ')"

rc=0
for t in $TARGETS; do
  if ! rustup target list --installed 2>/dev/null | grep -qx "$t"; then
    echo "  $t — NOT INSTALLED, skipping (rustup target add $t to cover it)"
    continue
  fi
  echo "  == $t"
  if cargo clippy --workspace --all-targets --target "$t" -- -D warnings 2>&1 | tail -40; then
    echo "     clean"
  else
    echo "     FAILED"
    rc=1
  fi
done

[ "$rc" = 0 ] && echo "clippygate: OK — clean on every installed CI target"
exit "$rc"
