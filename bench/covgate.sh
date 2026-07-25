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
COVJSON=$(mktemp)
# --output-path keeps the JSON pure: runners interleave rustup/cargo
# info lines into stdout, which broke stdout capture (2026-07-03).
COVLOG=$(mktemp)
# kevy-napi is excluded from the MEASUREMENT, not from testing: its only
# executable surface is N-API glue that needs a live Node runtime (the
# node_api symbols do not even link outside one), and ffigate runs its
# real suite via `node --test`. llvm-cov instrumenting a crate whose
# tests it can never execute only dilutes the workspace ratio.
cargo llvm-cov --workspace --exclude kevy-napi --lib --tests --summary-only --json \
    --output-path "$COVJSON" >"$COVLOG" 2>&1 || {
    echo "covgate: cargo llvm-cov run failed — last 40 lines:" >&2
    tail -40 "$COVLOG" >&2
    exit 2
}
rm -f "$COVLOG"
PCT=$(python3 -c "
import json
d = json.load(open('$COVJSON'))
print(f\"{d['data'][0]['totals']['lines']['percent']:.2f}\")
")
rm -f "$COVJSON"

if [ "$MODE" = "--update-baseline" ]; then
    printf '{\n  "workspace_line_coverage_pct": %s,\n  "recorded": "%s",\n  "note": "ratchet: only raise via --update-baseline after a deliberate improvement"\n}\n' \
        "$PCT" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$BASELINE"
    echo "covgate: baseline updated to ${PCT}%"
    exit 0
fi

[ -f "$BASELINE" ] || {
    echo "covgate: no baseline at $BASELINE — run with --update-baseline first" >&2
    exit 2
}
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
