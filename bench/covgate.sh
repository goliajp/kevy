#!/bin/bash
# covgate — the coverage regression gate (v2.1 五轴收口 cov axis).
#
#   bash bench/covgate.sh                    # gate against baseline
#   bash bench/covgate.sh --update-baseline  # record a new baseline
#
# Ratchet discipline (same as perfgate): the baseline only moves UP,
# and only via an explicit --update-baseline after a deliberate
# improvement lands. Never lower it to turn the gate green.
#
# Measures workspace line coverage via cargo-llvm-cov (external cargo
# tool — zero Cargo.toml dependencies, same class as miri/fuzz in CI).
# New-crate rule (kevy-index / kevy-text / kevy-vector when they land):
# >= 90% line coverage per NEW_CRATE_MIN below.
#
# Exit codes: 0 = PASS (or baseline updated), 1 = below baseline,
# 2 = refused (missing tools).
set -u

# llvm-cov instrumentation slows server boot by an order of magnitude, so
# the tests' wait budgets are scaled here rather than raised globally -- a
# normal build keeps its tight 60s bound and still fails fast on a real
# hang. See `patience()` in crates/kevy/tests/replication.rs. Bumped 3→6
# after spop_storm's replica (a full second runtime, in-process) still
# starved under the instrumented full-suite load at 180s.
export KEVY_TEST_PATIENCE=6

HERE=$(cd "$(dirname "$0")" && pwd)
BASELINE="$HERE/COV-BASELINE.json"
MODE=${1:-gate}
# Grace band: llvm-cov line counts wobble slightly across toolchains;
# a drop larger than this is a real regression.
TOLERANCE_PCT=${TOLERANCE_PCT:-0.30}

command -v cargo-llvm-cov >/dev/null 2>&1 || {
    echo "covgate: cargo-llvm-cov not installed (cargo install cargo-llvm-cov)" >&2
    exit 2
}

echo "covgate: measuring workspace line coverage (instrumented build + tests)..."
# One instrumented run, two instruments. deadgate needs the per-function
# region records this run already produces and --summary-only throws away;
# re-running the suite to get them would double a 25-minute CI job for data
# that was on the floor. When KEVY_COV_JSON names a path, the full export
# lands there and survives, and suite/corpus.toml declares this exact
# command so the percentage and the dead set are two readings of one run
# rather than two runs that disagree.
if [ -n "${KEVY_COV_JSON:-}" ]; then
    COVJSON="$KEVY_COV_JSON"
    SUMMARY_ONLY=""
    KEEP_JSON=1
else
    COVJSON=$(mktemp)
    SUMMARY_ONLY="--summary-only"
    KEEP_JSON=0
fi
# --output-path keeps the JSON pure: runners interleave rustup/cargo
# info lines into stdout, which broke stdout capture (2026-07-03).
COVLOG=$(mktemp)
# kevy-napi is excluded from the MEASUREMENT, not from testing: its only
# executable surface is N-API glue that needs a live Node runtime (the
# node_api symbols do not even link outside one), and ffigate runs its
# real suite via `node --test`. llvm-cov instrumenting a crate whose
# tests it can never execute only dilutes the workspace ratio.
cargo llvm-cov --workspace --exclude kevy-napi --lib --tests $SUMMARY_ONLY --json \
    --output-path "$COVJSON" >"$COVLOG" 2>&1 || {
    echo "covgate: cargo llvm-cov run failed — last 200 lines (a flaky replication test's panic prints well before the summary; 40 lines once truncated the crash away):" >&2
    tail -200 "$COVLOG" >&2
    exit 2
}
rm -f "$COVLOG"
PCT=$(python3 -c "
import json
d = json.load(open('$COVJSON'))
print(f\"{d['data'][0]['totals']['lines']['percent']:.2f}\")
")
[ "$KEEP_JSON" = "1" ] || rm -f "$COVJSON"
[ "$KEEP_JSON" = "1" ] && echo "covgate: full export kept at $COVJSON for deadgate"

HOST=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$HOST" in darwin) HOST=macos ;; esac

if [ "$MODE" = "--update-baseline" ]; then
    # Records the platform, and keeps what the old file said in prose. The
    # previous form wrote three fields and dropped the rest, so one update
    # from anywhere erased the only statement of where the number came
    # from — including `reference_macos`, which is the evidence that the
    # two platforms do not measure the same thing.
    PCT="$PCT" HOST="$HOST" BASELINE="$BASELINE" python3 - <<'PY'
import json, os, pathlib, datetime
p = pathlib.Path(os.environ["BASELINE"])
old = json.loads(p.read_text()) if p.exists() else {}
out = {
    "workspace_line_coverage_pct": float(os.environ["PCT"]),
    "platform": os.environ["HOST"],
    "recorded": datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
}
for k in ("recorded_on", "reference_macos", "note"):
    if k in old:
        out[k] = old[k]
p.write_text(json.dumps(out, indent=2) + "\n")
PY
    echo "covgate: baseline updated to ${PCT}% (platform ${HOST})"
    exit 0
fi

[ -f "$BASELINE" ] || {
    echo "covgate: no baseline at $BASELINE — run with --update-baseline first" >&2
    exit 2
}
# The baseline names the platform it was taken on, and this one has always
# been Linux — the file said so in prose and carried `reference_macos:
# 82.08` beside a Linux 79.64 as proof that the two do not measure the same
# thing. 2.44 points is many times any tolerance this gate would use, so a
# cross-platform comparison here is not a loose reading, it is a different
# question. setratchet already refuses exactly this; the oldest baseline in
# the tree did not.
BASE_PLATFORM=$(python3 -c "import json; print(json.load(open('$BASELINE')).get('platform',''))")
if [ -z "$BASE_PLATFORM" ]; then
    # A baseline that does not name its platform permits every platform,
    # which is the same permissive-on-missing-data shape this check was
    # added to close. --update-baseline writes the field; a file without
    # it predates that and cannot be compared against safely.
    echo "covgate: REFUSED — $BASELINE does not record the platform it was" >&2
    echo "  taken on, so nothing here can tell whether this comparison is" >&2
    echo "  like for like. Re-record it with --update-baseline where it belongs." >&2
    exit 2
fi
if [ "$BASE_PLATFORM" != "$HOST" ]; then
    echo "covgate: REFUSED — the baseline was recorded on ${BASE_PLATFORM} and this" >&2
    echo "  is ${HOST}. Coverage is not portable across them (this file records" >&2
    echo "  ${BASE_PLATFORM} $(python3 -c "import json;print(json.load(open('$BASELINE'))['workspace_line_coverage_pct'])")% beside a macOS reference of" >&2
    echo "  $(python3 -c "import json;print(json.load(open('$BASELINE')).get('reference_macos','?'))")%). Run it where the baseline lives, or record a new one there." >&2
    exit 2
fi
BASE=$(python3 -c "import json; print(json.load(open('$BASELINE'))['workspace_line_coverage_pct'])")
FLOOR=$(python3 -c "print(f'{$BASE - $TOLERANCE_PCT:.2f}')")

echo "covgate: measured ${PCT}% | baseline ${BASE}% | floor ${FLOOR}%"
PASS=$(python3 -c "print(1 if $PCT >= $FLOOR else 0)")
if [ "$PASS" = "1" ]; then
    echo "covgate: PASS"
    exit 0
fi
echo "covgate: FAIL — workspace line coverage ${PCT}% fell below floor ${FLOOR}% (baseline ${BASE}%)" >&2
exit 1
