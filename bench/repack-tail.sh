#!/usr/bin/env bash
# repack-tail — what the background index repack costs a client, and why
# a single repack step is sometimes slow.
#
# Writes split index leaves and never pack them; each shard's tick packs
# them afterwards, up to 500 µs a tick, four leaves between clock reads.
# While that runs the shard serves nothing. Two questions:
#
#   1. Client tail. The same workload against three servers, pinned the
#      same way, in alternating order over several rounds:
#        head      this tree, as shipped
#        norepack  this tree built with `harness-repack-off`: identical
#                  but for the repack, which never runs
#        v640      v6.4.0, before the packed-leaf index and its repack
#      Each run declares a range index, writes REPACK_ROWS hashes in
#      random value order (leaves split and stay part full), then at once
#      runs a paced mixed load for REPACK_SECS while the repack works:
#      HSET that moves an index entry, HGET, and IDX.QUERY RANGE ... LIMIT.
#      Latency is taken from when each request was due, per command, into
#      4%-wide log buckets (crates/kevy/examples/repack_load.rs).
#   2. Step time. The same run against a build with
#      `harness-repack-trace`, which times every step and tick and keeps
#      the slowest steps with what they did (leaves freed, entries moved,
#      buffers grown) and what the thread went through (CPU time, page
#      faults, voluntary and involuntary context switches). On glibc it
#      repeats under allocator tunables that turn off heap trimming and
#      fastbins, so a free-path cause shows up as a difference between
#      runs. Plus the same repack with no server at all
#      (examples/tidy_steps.rs), writes and cache pressure between steps.
#
# The analysis is bench/repack_tail_report.py; it runs at the end and its
# output lands in <out>/summary.txt.
#
# Knobs (environment):
#   REPACK_ROWS=2000000       rows written before the load
#   REPACK_SECS=60            load window per run, seconds
#   REPACK_ROUNDS=3           rounds of head / norepack / v640, order rotating
#   REPACK_RATE=30000         requests per second across all connections
#   REPACK_CONNS=6            load connections, one thread each
#   REPACK_MIX=30,60,10       write, read, query weights
#   REPACK_THREADS=2          server shards
#   REPACK_SERVER_CPUS=4-5    taskset list for the server (Linux only)
#   REPACK_CLIENT_CPUS=0-3,6-7  taskset list for the client (Linux only)
#   REPACK_PORT=7063
#   REPACK_TRACE_US=100       a step at or over this is kept whole
#   REPACK_TRACE_VARIANTS     trace runs: plain, notrim, nofastbin
#                             (default: all three on glibc, plain elsewhere)
#   REPACK_SERVERS="head norepack v640"
#   REPACK_V64_BIN=           a v6.4.0 kevy binary to use instead of building
#   REPACK_OUT=target/repack-tail/<host>-<time>   results directory
#   REPACK_FAST_BUILD=0       1: build without LTO (for trying the harness,
#                             never for numbers)
#   REPACK_IDLE_MIN=80        refuse unless the box is this % idle (Linux)
#   REPACK_BUILD_ONLY=0       1: build the binaries and stop
#
# Runtime at the defaults on an 8-core box: about 13 runs of ~80 s
# (1–2 s start, ~10 s fill, 60 s load, teardown) plus ~30 s without the
# server: about 20 minutes. Building adds four release builds the first
# time (this tree three ways, v6.4.0 once); they are kept under
# target/repack-tail/bin-<tree> and reused while the tree is unchanged.
#
# Exit: 0 done, 1 a run failed, 2 refused (busy box, leftovers, no tag).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
ROOT=$PWD

ROWS=${REPACK_ROWS:-2000000}
SECS=${REPACK_SECS:-60}
ROUNDS=${REPACK_ROUNDS:-3}
RATE=${REPACK_RATE:-30000}
CONNS=${REPACK_CONNS:-6}
MIX=${REPACK_MIX:-30,60,10}
THREADS=${REPACK_THREADS:-2}
SCPUS=${REPACK_SERVER_CPUS:-4-5}
CCPUS=${REPACK_CLIENT_CPUS:-0-3,6-7}
PORT=${REPACK_PORT:-7063}
TRACE_US=${REPACK_TRACE_US:-100}
SERVERS=${REPACK_SERVERS:-head norepack v640}
IDLE_MIN=${REPACK_IDLE_MIN:-80}
OUT=${REPACK_OUT:-$ROOT/target/repack-tail/$(hostname -s)-$(date +%Y%m%d-%H%M%S)}
GLIBC=0
ldd --version 2>&1 | grep -qiE 'glibc|gnu libc' && GLIBC=1
if [ "$GLIBC" = 1 ]; then
    VARIANTS=${REPACK_TRACE_VARIANTS:-plain notrim nofastbin}
else
    VARIANTS=${REPACK_TRACE_VARIANTS:-plain}
fi

SPIN=""
CPIN=""
if command -v taskset >/dev/null 2>&1; then
    SPIN="taskset -c $SCPUS"
    CPIN="taskset -c $CCPUS"
fi

# ---- the box --------------------------------------------------------------

LEFTOVER=$(pgrep -fl 'kevy-(head|norepack|trace|v640)|repack_load|tidy_steps' | grep -v pgrep || true)
if [ -n "$LEFTOVER" ]; then
    echo "repack-tail: REFUSED — leftover harness processes:" >&2
    echo "$LEFTOVER" >&2
    exit 2
fi
box_idle() {
    local a b
    a=$(awk '/^cpu /{print $2+$3+$4+$6+$7+$8, $5}' /proc/stat); sleep 1
    b=$(awk '/^cpu /{print $2+$3+$4+$6+$7+$8, $5}' /proc/stat)
    echo "$a $b" | awk '{busy=$3-$1; idle=$4-$2; print int(100*idle/(busy+idle))}'
}
if [ "${REPACK_BUILD_ONLY:-0}" != 1 ]; then
    if [ -r /proc/stat ]; then
        IDLE0=$(box_idle)
        if [ "$IDLE0" -lt "$IDLE_MIN" ]; then
            echo "repack-tail: REFUSED — box ${IDLE0}% idle (< ${IDLE_MIN}%). A tail read on a busy box measures the neighbours." >&2
            exit 2
        fi
        echo "repack-tail: box ${IDLE0}% idle at start"
    else
        echo "repack-tail: no /proc/stat — no idle check, no pinning; numbers from here are for trying the harness only"
    fi
fi

# ---- binaries -------------------------------------------------------------

TREE=$(git rev-parse --short=12 HEAD)
DIRTY=$(git status --porcelain -- crates Cargo.toml Cargo.lock | wc -l | tr -d ' ')
[ "$DIRTY" != 0 ] && TREE="$TREE-dirty$(git diff HEAD -- crates Cargo.toml Cargo.lock | cksum | cut -d' ' -f1)"
FLAVOUR=release
if [ "${REPACK_FAST_BUILD:-0}" = 1 ]; then
    export CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
    FLAVOUR=fast
fi
BIN=$ROOT/target/repack-tail/bin-$TREE-$FLAVOUR
mkdir -p "$BIN"

build_this() { # $1 = name, $2 = features ("" for none), $3... = extra targets
    local name=$1 feat=$2; shift 2
    [ -x "$BIN/kevy-$name" ] && return 0
    echo "repack-tail: building kevy-$name ($FLAVOUR)"
    cargo build -q --release --locked -p kevy --bin kevy ${feat:+--features "$feat"} "$@" || return 1
    cp target/release/kevy "$BIN/kevy-$name"
}
build_v640() {
    [ -n "${REPACK_V64_BIN:-}" ] && { cp "$REPACK_V64_BIN" "$BIN/kevy-v640"; return 0; }
    local cache=$ROOT/target/repack-tail/kevy-v640-$FLAVOUR
    if [ ! -x "$cache" ]; then
        git rev-parse -q --verify v6.4.0 >/dev/null || {
            echo "repack-tail: REFUSED — no v6.4.0 tag here; fetch tags or set REPACK_V64_BIN" >&2; exit 2; }
        local src=$ROOT/target/repack-tail/src-v640
        rm -rf "$src"; mkdir -p "$src"
        git archive v6.4.0 | tar -x -C "$src" || return 1
        echo "repack-tail: building kevy v6.4.0 ($FLAVOUR)"
        (cd "$src" && CARGO_TARGET_DIR=$ROOT/target/repack-tail/target-v640 \
            cargo build -q --release --locked -p kevy --bin kevy) || return 1
        cp "$ROOT/target/repack-tail/target-v640/release/kevy" "$cache"
        rm -rf "$src"
    fi
    cp "$cache" "$BIN/kevy-v640"
}
if [ ! -x "$BIN/repack_load" ]; then
    rm -f "$BIN/kevy-head"
    build_this head "" --example repack_load || exit 1
    cp target/release/examples/repack_load "$BIN/repack_load"
fi
build_this norepack harness-repack-off || exit 1
if [ ! -x "$BIN/tidy_steps" ]; then
    rm -f "$BIN/kevy-trace"
    build_this trace harness-repack-trace --example tidy_steps || exit 1
    cp target/release/examples/tidy_steps "$BIN/tidy_steps"
fi
case " $SERVERS " in *" v640 "*) build_v640 || exit 1 ;; esac
[ "${REPACK_BUILD_ONLY:-0}" = 1 ] && { echo "repack-tail: binaries in $BIN"; exit 0; }

# ---- runs -----------------------------------------------------------------

mkdir -p "$OUT"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/repack-tail-XXXXXX")
SRV=""
cleanup() {
    [ -n "$SRV" ] && { kill -9 "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; }
    rm -rf "$WORK"
}
trap cleanup EXIT
LOAD=$BIN/repack_load
{
    echo "host=$(hostname -s) uname=$(uname -srm) cpus=$(getconf _NPROCESSORS_ONLN) tree=$TREE build=$FLAVOUR"
    echo "rows=$ROWS secs=$SECS rounds=$ROUNDS rate=$RATE conns=$CONNS mix=$MIX threads=$THREADS"
    echo "server_cpus=${SPIN:+$SCPUS} client_cpus=${CPIN:+$CCPUS} trace_us=$TRACE_US variants=$VARIANTS"
    for b in "$BIN"/kevy-*; do echo "bin $(basename "$b") $(cksum < "$b" | cut -d' ' -f1)"; done
} > "$OUT/meta.txt"
cat "$OUT/meta.txt"

# Jiffies the whole box spent busy, and jiffies our own processes spent;
# their difference over the load window is what everything else ran.
busy_jiffies() { awk '/^cpu /{print $2+$3+$4+$7+$8}' /proc/stat 2>/dev/null || echo 0; }
own_jiffies() {
    local t=0 p
    for p in "$@"; do
        [ -r "/proc/$p/stat" ] && t=$((t + $(awk '{print $14+$15+$16+$17}' "/proc/$p/stat")))
    done
    echo $t
}

variant_env() {
    case $1 in
        plain) echo "" ;;
        notrim) echo "GLIBC_TUNABLES=glibc.malloc.trim_threshold=1099511627776" ;;
        nofastbin) echo "GLIBC_TUNABLES=glibc.malloc.mxfast=0" ;;
        *) echo "repack-tail: unknown trace variant $1" >&2; exit 2 ;;
    esac
}

one_run() { # $1 = tag, $2 = binary name, $3 = extra env (may be empty)
    local tag=$1 bin=$BIN/kevy-$2 extra=$3 lp b0 o0 b1 o1 hz
    local dir=$WORK/data-$tag
    rm -rf "$dir"; mkdir -p "$dir"
    # shellcheck disable=SC2086
    env KEVY_BIND=127.0.0.1 KEVY_REPACK_TRACE="$OUT/$tag.trace" KEVY_REPACK_TRACE_US="$TRACE_US" $extra \
        $SPIN "$bin" --port "$PORT" --threads "$THREADS" --dir "$dir" --no-aof >"$OUT/$tag.srv.log" 2>&1 &
    SRV=$!
    $CPIN "$LOAD" ping port="$PORT" || { echo "  ✗ $tag: server did not answer"; tail -3 "$OUT/$tag.srv.log"; return 1; }
    $CPIN "$LOAD" fill port="$PORT" rows="$ROWS" conns=4 > "$OUT/$tag.fill" || return 1
    $CPIN "$LOAD" load port="$PORT" rows="$ROWS" secs="$SECS" rate="$RATE" conns="$CONNS" mix="$MIX" \
        out="$OUT/$tag.json" > "$OUT/$tag.load" &
    lp=$!
    # CPU the rest of the box used during the window, in % of one core,
    # read while the client still runs so its own share can be taken out
    local win=$(( SECS > 3 ? SECS - 2 : 1 ))
    sleep 1
    b0=$(busy_jiffies); o0=$(own_jiffies "$SRV" "$lp")
    sleep "$win"
    b1=$(busy_jiffies); o1=$(own_jiffies "$SRV" "$lp")
    wait "$lp" || { echo "  ✗ $tag: load client failed"; return 1; }
    hz=$(getconf CLK_TCK)
    [ -r /proc/stat ] && echo "foreign_cpu_pct=$(( ( (b1 - b0) - (o1 - o0) ) * 100 / (hz * win) ))" >> "$OUT/$tag.load"
    sleep 1.5 # the trace file is rewritten once a second
    kill -9 "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; SRV=""
    rm -rf "$dir"
    echo "  $tag: $(head -1 "$OUT/$tag.fill")"
    sed -n '1,4p' "$OUT/$tag.load" | sed 's/^/    /'
}

read -r -a order <<< "$SERVERS"
n=${#order[@]}
for r in $(seq 1 "$ROUNDS"); do
    echo "round $r/$ROUNDS"
    for i in $(seq 0 $((n - 1))); do
        s=${order[$(( (i + r - 1) % n ))]}
        one_run "r$r-$s" "$s" "" || exit 1
    done
done
for v in $VARIANTS; do
    echo "trace run: $v"
    one_run "trace-$v" trace "$(variant_env "$v")" || exit 1
done
echo "the repack with no server around it"
$SPIN "$BIN/tidy_steps" rows="$ROWS" writes=50 evict_mb=16 long_us="$TRACE_US" max_steps=$((ROWS / 20)) \
    > "$OUT/lib.txt" || exit 1
head -2 "$OUT/lib.txt" | sed 's/^/  /'

python3 bench/repack_tail_report.py "$OUT" | tee "$OUT/summary.txt"
echo "repack-tail: results in $OUT"
