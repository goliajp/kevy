#!/usr/bin/env bash
# deadgate — the never-executed set may only shrink.
#
#   bash bench/deadgate.sh                    # gate against the baseline
#   bash bench/deadgate.sh --update-baseline  # record (refuses a worse set)
#
# covgate holds a percentage. This holds the identities behind it: coverage
# can sit at 79.64% for a year while the identity of the uncovered fifth is
# completely replaced, and the number never moves. Here every symbol that
# owns a never-executed region is named, and none may gain or join.
#
# The baseline is only ever recorded on the enforcing platform. Code
# switched off by cfg is ABSENT from a coverage run rather than dead in it —
# a macOS run sees 1 of 16 uring_*.rs files and none of kevy-uring — so a
# cross-platform comparison makes whole symbols leave the set, which a
# ratchet reads as improvement. setratchet's identity check refuses that
# comparison; this script does not need to remember it.
#
# Exit: 0 PASS/recorded, 1 the set grew, 2 refused.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
BASELINE="$HERE/DEAD-BASELINE.json"
OBSERVED="$HERE/DEAD-SET.json"
COV="${KEVY_COV_JSON:-$ROOT/target/llvm-cov-c1.json}"
# The corpus command, kept in one place. Changing it here without changing
# suite/corpus.toml is how two instruments start answering different
# questions while both look healthy.
CORPUS_ARGS="--workspace --exclude kevy-napi --lib --tests"
MODE=${1:-gate}

command -v cargo >/dev/null || { echo "deadgate: REFUSED — no cargo" >&2; exit 2; }

if [ ! -f "$COV" ]; then
  echo "deadgate: producing the corpus run (this is the slow part)"
  # shellcheck disable=SC2086
  KEVY_TEST_PATIENCE=6 cargo llvm-cov $CORPUS_ARGS \
      --json --output-path "$COV" || {
    echo "deadgate: REFUSED — the corpus run failed; there is nothing to measure" >&2
    exit 2
  }
fi
[ -s "$COV" ] || { echo "deadgate: REFUSED — $COV is empty" >&2; exit 2; }

python3 "$ROOT/tools/coverage_atlas.py" "$COV" || exit $?

if [ "$MODE" = "--update-baseline" ]; then
  shift
  exec python3 "$ROOT/tools/setratchet.py" update "$BASELINE" "$OBSERVED" "$@"
fi
[ -f "$BASELINE" ] || {
  echo "deadgate: REFUSED — no $BASELINE. Record one on the enforcing" >&2
  echo "  platform first: bash bench/deadgate.sh --update-baseline" >&2
  exit 2
}
exec python3 "$ROOT/tools/setratchet.py" gate "$BASELINE" "$OBSERVED"
