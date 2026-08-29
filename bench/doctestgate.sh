#!/usr/bin/env bash
# doctestgate — what the documentation owes may only shrink.
#
#   bash bench/doctestgate.sh                    # gate against the baseline
#   bash bench/doctestgate.sh --update-baseline  # record (refuses a worse set)
#
# 2,661 public items, 93.9% carrying prose and 0.7% carrying an executable
# example. So this does not gate on documentation coverage, which is close
# to finished; it gates on the deficit — items owed a doc and items owed a
# doctest — because a paragraph is a promise nobody checked and a doctest is
# compiled and run.
#
# The measurement is rustdoc's own coverage mode, not a scan for `///`,
# which would count a comment documenting nothing and miss #[doc = ...].
# That needs nightly, and REFUSING is the right answer when it is absent:
# an unmeasured deficit is not a met one.
#
# Exit: 0 PASS/recorded, 1 the deficit grew, 2 refused.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
BASELINE="$HERE/DOC-BASELINE.json"
OBSERVED="$HERE/DOC-DEFICIT.json"
MODE=${1:-gate}

rustup run nightly rustdoc --version >/dev/null 2>&1 || {
  echo "doctestgate: REFUSED — rustdoc coverage needs nightly; install with" >&2
  echo "  rustup toolchain install nightly" >&2
  exit 2
}

RUSTDOCFLAGS='-Z unstable-options --show-coverage' \
  cargo +nightly doc --workspace --no-deps >/dev/null 2>&1 || {
  echo "doctestgate: REFUSED — the rustdoc coverage pass failed" >&2
  exit 2
}

python3 "$ROOT/tools/doc_deficit.py" --out "$OBSERVED" || exit $?

if [ "$MODE" = "--update-baseline" ]; then
  shift
  exec python3 "$ROOT/tools/setratchet.py" update "$BASELINE" "$OBSERVED" "$@"
fi
[ -f "$BASELINE" ] || {
  echo "doctestgate: REFUSED — no $BASELINE. Record one first:" >&2
  echo "  bash bench/doctestgate.sh --update-baseline" >&2
  exit 2
}
exec python3 "$ROOT/tools/setratchet.py" gate "$BASELINE" "$OBSERVED"
