#!/usr/bin/env bash
# availgate-loop — instrumented-reproduction driver for the failover
# convergence wedge (the availgate three-time flake). Runs availgate in
# a loop under CPU contention with the replication trace probes on,
# stopping at the first failure with the crime scene preserved.
#
# Every round's full output goes to its own file — never through a
# grep/tail pipe (a piped verdict has eaten the crime scene before).
# The verdict is the exit code, nothing else.
#
# Usage: bash bench/availgate-loop.sh <kevy-binary> <rounds> <keep-dir> [contenders]
set -u
KBIN=${1:?usage: availgate-loop.sh <kevy-binary> <rounds> <keep-dir> [contenders]}
ROUNDS=${2:?rounds}
KEEP=${3:?keep-dir}
CONTENDERS=${4:-$(( $(nproc 2>/dev/null || sysctl -n hw.ncpu) * 2 ))}
mkdir -p "$KEEP"

# CPU contention: plain shell busy-loops, 2x core oversubscription by
# default. The box passed availgate x10 idle after the generation fix,
# so scheduler pressure is part of the reproduction recipe.
CPIDS=()
cleanup() { kill "${CPIDS[@]:-}" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT
for _ in $(seq "$CONTENDERS"); do
    ( while :; do :; done ) &
    CPIDS+=($!)
done

export KEVY_DEBUG_REPL_TRACE=1
export KEVY_AVAILGATE_KEEP="$KEEP"
for i in $(seq "$ROUNDS"); do
    echo "=== availgate-loop round $i/$ROUNDS (contenders=$CONTENDERS) ==="
    if ! bash "$(dirname "$0")/availgate.sh" "$KBIN" > "$KEEP/round-$i.log" 2>&1; then
        echo "availgate-loop: FAIL on round $i — log at $KEEP/round-$i.log"
        exit 1
    fi
    echo "availgate-loop: round $i ok"
    # Inter-run settle: availgate binds fixed ports; give TIME_WAIT and
    # straggler processes a moment (the earlier x10 loop's AddrInUse
    # artifacts came from skipping this).
    sleep 2
done
echo "availgate-loop: all $ROUNDS rounds green"
