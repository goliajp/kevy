#!/usr/bin/env bash
# doctestrun — the examples in the documentation are compiled and run.
#
#   bash bench/doctestrun.sh
#
# doctestgate counts how many public items carry an executable example, and
# its whole argument for preferring examples to prose is that "a paragraph is
# a promise nobody checked and a doctest is compiled and run".
#
# Nothing in this repository ran them. Every test invocation here — CI, the
# release workflow, the suite — spells `cargo test --workspace --lib --tests`,
# and that pair is exactly the combination that EXCLUDES doctests. The string
# `--doc` appeared nowhere in the tree. So the examples the bar demands were
# compiled by nobody, and one that had stopped building would have sat there
# indefinitely while a gate counted it as verification.
#
# The vacuity guard matters more than the run. A doctest harness that collects
# nothing exits 0 and prints "0 passed" — the exact shape of a clean bill of
# health. So this REFUSES on an empty collection, and checks what cargo
# collected against a witness sharing none of its machinery: a scan for
# runnable code fences inside doc comments. If the tree plainly carries
# examples and cargo ran none, the apparatus is broken, not the code.
#
# Exit: 0 every example ran and passed, 1 one failed, 2 refused.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
cd "$ROOT" || exit 2

# The witness. Pairs fences per file so an opening tag is read from the
# opening line only, and counts just the tags rustdoc will build: bare,
# `rust`, `no_run`, `should_panic`, `compile_fail`, `edition*`. The tree also
# carries ```text diagrams and ```sh transcripts, which are not examples and
# must not inflate the floor.
WITNESS_AWK=$(mktemp)
trap 'rm -f "$WITNESS_AWK" "${LOG:-}"' EXIT
cat > "$WITNESS_AWK" <<'AWKEOF'
FNR==1 { inblk = 0 }
/^[[:space:]]*(\/\/\/|\/\/!)[[:space:]]*```/ {
  if (inblk) { inblk = 0; next }
  inblk = 1
  tag = $0
  sub(/^[[:space:]]*(\/\/\/|\/\/!)[[:space:]]*```/, "", tag)
  gsub(/[[:space:],]/, "", tag)
  if (tag == "" || tag == "rust" || tag ~ /^(no_run|should_panic|compile_fail|edition)/) n++
}
END { print n+0 }
AWKEOF
WITNESS=$(find crates -name '*.rs' -print0 | xargs -0 awk -f "$WITNESS_AWK" \
          | awk '{s+=$1} END{print s+0}')

LOG=$(mktemp)
cargo test --workspace --doc >"$LOG" 2>&1
RC=$?

# Named rather than positional. Splitting a summary line on [ ;] puts an
# EMPTY field where the failure count looks like it belongs — the number
# lands one further along — so a positional read reports zero failures
# however many there are. That is the exact shape of green this gate exists
# to refuse, and it was in this script until it was tested with a failure.
PASSED=$(grep '^test result' "$LOG" | sed -E 's/.*[^0-9]([0-9]+) passed.*/\1/' \
         | awk '{s+=$1} END{print s+0}')
FAILED=$(grep '^test result' "$LOG" | sed -E 's/.*[^0-9]([0-9]+) failed.*/\1/' \
         | awk '{s+=$1} END{print s+0}')
RAN=$((PASSED + FAILED))

# Order matters. A failure inside an early crate makes cargo abandon the
# rest, so RAN drops — and a floor checked first would report a broken
# apparatus for what is a broken example. The run's own verdict is read
# before anything is concluded about how much of it happened.
if [ "$RC" -ne 0 ] || [ "$FAILED" -ne 0 ]; then
  echo "doctestrun: FAIL — $FAILED of $RAN examples do not run" >&2
  grep -E '^(---- .* stdout|error|failures:)' "$LOG" | head -40 >&2
  exit 1
fi

if [ "$RAN" -eq 0 ]; then
  echo "doctestrun: REFUSED — cargo reported success having collected 0" >&2
  echo "  doctests, while the tree carries $WITNESS runnable doc-comment" >&2
  echo "  examples. An empty collection is not a clean bill of health." >&2
  exit 2
fi

# Three quarters, not equality: a platform-gated crate contributes fences the
# host cannot compile (kevy-uring alone is Linux-only), so the two counts
# legitimately part. This catches the collector going dark; it does not
# police the ratio.
FLOOR=$(( WITNESS * 3 / 4 ))
if [ "$RAN" -lt "$FLOOR" ]; then
  echo "doctestrun: REFUSED — every example that ran passed, but only $RAN ran" >&2
  echo "  against $WITNESS runnable examples in the tree (floor $FLOOR)." >&2
  echo "  Too few were collected to call this a pass." >&2
  exit 2
fi

echo "doctestrun: PASS — $RAN examples compiled and ran (witness: $WITNESS runnable fences)"
