#!/usr/bin/env bash
# nostdgate — compile the no_std stones the way CI does, before CI does.
#
# `kevy-store` builds without `std` for the embedded targets, and nothing on
# a developer machine exercises that: `cargo check -p kevy-store` uses the
# default features, so a file that reaches for `std::sync::Arc` or a bare
# `Vec` compiles perfectly here and fails only in CI. That is exactly how
# `packed_row.rs` shipped ten errors into the `iot` job — and it was caught
# after the branch had already merged.
#
# The commands are read out of `.github/workflows/ci.yml` rather than
# restated, so a target or feature set added there cannot leave this behind,
# and finding none is a refusal rather than a pass.
set -euo pipefail
cd "$(dirname "$0")/.."

CI=.github/workflows/ci.yml
[ -f "$CI" ] || { echo "nostdgate: REFUSED — no $CI to read the checks from" >&2; exit 2; }

# Every `cargo check --target <triple> ... --no-default-features ...` line in
# the workflow. One per line, whitespace-normalised.
# `mapfile` is bash 4; this runs on a Mac's bash 3.2 too, so read a
# newline-separated list the portable way.
CHECKS=()
while IFS= read -r line; do
  [ -n "$line" ] && CHECKS+=("$line")
done < <(grep -oE 'cargo check --target [a-z0-9_-]+ -p [a-z-]+ --no-default-features --features [a-z,-]+' "$CI" | sort -u)

if [ ${#CHECKS[@]} -eq 0 ]; then
  echo "nostdgate: REFUSED — parsed no no_std checks out of $CI." >&2
  echo "The workflow's shape changed; a gate that finds nothing must not read as PASS." >&2
  exit 2
fi

fail=0
for c in "${CHECKS[@]}"; do
  triple=$(echo "$c" | awk '{print $4}')
  if ! rustup target list --installed 2>/dev/null | grep -qx "$triple"; then
    echo "nostdgate: SKIP — $triple is not installed (rustup target add $triple)"
    continue
  fi
  echo "  $c"
  if ! $c > /tmp/nostdgate.$$ 2>&1; then
    echo "  ✗ FAILED:"
    grep -E "^error" /tmp/nostdgate.$$ | head -5 | sed 's/^/      /'
    fail=1
  fi
  rm -f /tmp/nostdgate.$$
done

[ $fail -eq 0 ] || { echo "nostdgate: FAIL — a no_std check above is broken"; exit 1; }
echo "nostdgate: PASS — ${#CHECKS[@]} no_std check(s) from $CI compile"
