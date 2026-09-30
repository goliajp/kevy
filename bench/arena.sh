#!/usr/bin/env bash
# arena — kevy against valkey, Redis and Dragonfly: the published table.
#
#   bash bench/arena.sh <kevy-binary> [ROUNDS]     # ROUNDS defaults to 3
#
# Each round runs every engine on the same cores, one at a time, through
# the cells get set incr lpush sadd hset zadd. The table is the per-cell
# median over every window of every round, each kevy / other ratio with a
# 99% paired bootstrap interval over those windows, and the cost per op
# (instructions user and kernel, cycles, syscalls, engine cpus) from the
# same windows. It is run once per release, when the numbers are published;
# if a round was disturbed (the fgn column, the notes), run it again.
#
#   - topology: the engine gets 4 cores on CPUs 0-3 (4 threads / io-threads /
#     proactor threads), their SMT siblings 8-11 stay empty, the load gets both
#     threads of cores 4-7 (8 threads);
#   - throughput is read from the SERVER's command counter over a wall window
#     timed here, NOT from redis-benchmark's rate: under `--threads` the
#     benchmark exits on its own 250ms showThroughput timer
#     (redis-benchmark.c:52, :1653; without --threads it stops in clientDone
#     at :425), so its rate is quantized to N/(k*250ms) and understated. Every
#     engine exposes the same counter, so the comparison stays like-for-like;
#   - after a cell's windows, one more window with 16 load threads: if it
#     beats the cell's best window by more than 2%, the load generator was the
#     limit and the cell is CLIENT-BOUND, not a result;
#   - competitor versions and image digests, and the box's own settings, are
#     in the output header. The versions come from
#     bench/COMPETITOR-ANCHORS.json; each image is asked what it actually is,
#     and a mismatch stops the run.
#
# Knobs: CONC (50), PIPE (16), WINDOW seconds (3), RUNS windows per cell (5),
# FOURWAY=0 for kevy against valkey only. CONC=1 PIPE=1 is the single
# connection round trip.
#
# ROOT: arena runs as root. It needs docker for the competitors, docker on
# the bench box is root-only, and rootless cannot substitute: `--cpuset-cpus`
# needs the cpuset controller delegated to the user slice, and it is not, so
# a rootless run would silently lose the core pinning. It never calls pkill:
# it kills the pid it spawned and removes the containers it named, and none
# of its `docker run` invocations mount a host path. Being root is also what
# lets it attach perf to the containerised engines.
set -u
. "$(dirname "$0")/bench-lock.sh"   # hold the machine's bench lock for the whole run

KBIN=${1:?usage: arena.sh <kevy-binary> [ROUNDS]}
KBIN=$(cd "$(dirname "$KBIN")" && pwd)/$(basename "$KBIN")
ROUNDS=${2:-3}
cd "$(dirname "$0")" || exit 2

SRV_CORES=0-3
SRV_THREADS=4
CLI_CORES=4-7,12-15
CLI_THREADS=8
PROBE_THREADS=16
# N only has to keep the load generator busy through the windows at the
# slowest engine and cell; the generator is killed once they close
N=${N:-2000000000}
RAMP=${RAMP:-1}
WINDOW=${WINDOW:-3}
CONC=${CONC:-50}
PIPE=${PIPE:-16}
RUNS=${RUNS:-5}
PORT=7201
TESTS="get set incr lpush sadd hset zadd"
SAMPLES=$(mktemp)
trap 'rm -f "$SAMPLES"' EXIT

. ./anchor-lib.sh   # cwd is this script's directory, set above

VALKEY_PIN=$(anchor_pin valkey)
VALKEY_VER=$(anchor_image_ver "valkey/valkey:$VALKEY_PIN" valkey-server --version)
anchor_require valkey "$VALKEY_PIN" "$VALKEY_VER"
ENGINES="valkey $VALKEY_VER"
IMAGES="valkey/valkey:$VALKEY_PIN"
if [ "${FOURWAY:-1}" = 1 ]; then
    REDIS_PIN=$(anchor_pin redis)
    REDIS_VER=$(anchor_image_ver "redis:$REDIS_PIN" redis-server --version)
    anchor_require redis "$REDIS_PIN" "$REDIS_VER"
    DRAGONFLY_PIN=$(anchor_pin dragonfly)
    DRAGONFLY_VER=$(anchor_image_ver "docker.dragonflydb.io/dragonflydb/dragonfly:v$DRAGONFLY_PIN" --version)
    anchor_require dragonfly "$DRAGONFLY_PIN" "$DRAGONFLY_VER"
    ENGINES="redis $REDIS_VER | valkey $VALKEY_VER | dragonfly $DRAGONFLY_VER"
    IMAGES="$IMAGES redis:$REDIS_PIN docker.dragonflydb.io/dragonflydb/dragonfly:v$DRAGONFLY_PIN"
fi

# Settings that move every engine's number and are nobody's code: printed so a
# table can be matched to the box it came from. Absent facts print as "—".
box_facts() {
    local img
    echo "# kernel: $(uname -r)"
    echo "# mitigations: $(grep -oE 'mitigations=[^ ]+' /proc/cmdline || echo 'kernel default')"
    echo "# governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo —)"
    echo "# no_turbo: $(cat /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null || echo —)"
    echo "# audit: $(auditctl -s 2>/dev/null | awk '/^enabled/ {print "enabled " $2}' | grep . || echo —)"
    echo "# nft ruleset sha256: $(nft list ruleset 2>/dev/null | sha256sum | cut -c1-16)"
    echo "# perf: $(perf --version 2>&1 | head -1)"
    echo "# redis-benchmark: $(redis-benchmark --version 2>&1 | head -1)"
    for img in $IMAGES; do
        echo "# image $img: $(docker image inspect --format '{{index .RepoDigests 0}}' "$img" 2>/dev/null || echo —)"
    done
}

wait_ready() {
    for _ in $(seq 1 100); do
        [ "$(redis-cli -h 127.0.0.1 -p "$PORT" PING 2>/dev/null)" = PONG ] && return 0
        sleep 0.1
    done
    echo "!! port $PORT never came up" >&2
    return 1
}

# One window. $1 engine, $2 verb, $3 round (0 = the headroom probe), $4
# window, $5 pid:threads of the generator.
arena_window() {
    python3 perfgate_measure.py window --angle "$2" --side "$1" --obs "$3" --win "$4" \
        --srv-pid "$EPID" --srv-cpus "$SRV_CORES" --port "$PORT" --secs "$WINDOW" --gens "$5"
}

start_load() { # $1 test, $2 threads
    taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p "$PORT" \
        -t "$1" -n "$N" -c "$CONC" -P "$PIPE" --threads "$2" -q >/dev/null 2>&1 &
    BPID=$!
    sleep "$RAMP"
}

stop_load() { kill "$BPID" 2>/dev/null; wait "$BPID" 2>/dev/null; }

# RUNS windows of one cell, then the headroom probe.
bench_cell() { # $1 engine, $2 test, $3 round
    local verb win out
    verb=$(echo "$2" | tr '[:lower:]' '[:upper:]')
    start_load "$2" "$CLI_THREADS"
    for win in $(seq 1 "$RUNS"); do
        out=$(arena_window "$1" "$verb" "$3" "$win" "$BPID:$CLI_THREADS") || { stop_load; return 2; }
        echo "$out" >>"$SAMPLES"
    done
    stop_load
    start_load "$2" "$PROBE_THREADS"
    out=$(arena_window "$1" "$verb" 0 "$3" "$BPID:$PROBE_THREADS") || { stop_load; return 2; }
    stop_load
    echo "$out" >>"$SAMPLES"
}

engine_pid() { # $1 label, $2 pid we spawned
    if [ "$1" = kevy ]; then echo "$2"
    else docker inspect -f '{{.State.Pid}}' "arena-$1"; fi
}

run_engine() { # round, label, start-command...
    local round=$1 label=$2 spid t rc=0
    shift 2
    "$@" >/dev/null 2>&1 &
    spid=$!
    sleep 1
    if ! wait_ready; then
        # a missing engine is a hole in the table and must read as one
        echo "# !! $label ABSENT in round $round (did not start)"
        kill "$spid" 2>/dev/null
        docker rm -f "arena-$label" >/dev/null 2>&1 || true
        return 0
    fi
    EPID=$(engine_pid "$label" "$spid")
    for t in $TESTS; do
        bench_cell "$label" "$t" "$round" || { rc=$?; break; }
    done
    kill "$spid" 2>/dev/null
    wait "$spid" 2>/dev/null
    docker rm -f "arena-$label" >/dev/null 2>&1 || true
    [ "$rc" -eq 0 ] || exit "$rc"
    sleep 1
}

echo "# arena — $(date -u +%F) — $($KBIN --version | head -1)"
echo "# engines: $ENGINES"
echo "# protocol: -c $CONC -P $PIPE, engine cpus $SRV_CORES ($SRV_THREADS threads), cpus 8-11 idle, load cpus $CLI_CORES ($CLI_THREADS threads), $ROUNDS rounds x $RUNS windows of ${WINDOW}s after a ${RAMP}s ramp"
echo "# measured: server-side total_commands_processed and perf stat -p on the engine, same window (NOT redis-benchmark's rate)"
box_facts
echo "# note: the redis-benchmark -t cells use one fixed key per command type"

for round in $(seq 1 "$ROUNDS"); do
    echo "arena: round $round/$ROUNDS" >&2
    run_engine "$round" kevy \
        taskset -c "$SRV_CORES" env KEVY_BIND=127.0.0.1 "$KBIN" --threads "$SRV_THREADS" --port $PORT --no-aof
    run_engine "$round" valkey \
        docker run --rm --name arena-valkey --network host --cpuset-cpus "$SRV_CORES" \
        "valkey/valkey:$VALKEY_PIN" valkey-server --port $PORT --save '' --appendonly no --io-threads "$SRV_THREADS"
    if [ "${FOURWAY:-1}" = 1 ]; then
        run_engine "$round" redis8 \
            docker run --rm --name arena-redis8 --network host --cpuset-cpus "$SRV_CORES" \
            "redis:$REDIS_PIN" redis-server --port $PORT --save '' --appendonly no --io-threads "$SRV_THREADS"
        run_engine "$round" dragonfly \
            docker run --rm --name arena-dragonfly --network host --cpuset-cpus "$SRV_CORES" \
            --ulimit memlock=-1 "docker.dragonflydb.io/dragonflydb/dragonfly:v$DRAGONFLY_PIN" \
            --port $PORT --proactor_threads="$SRV_THREADS"
    fi
done

python3 arena_table.py COMPETITOR-ANCHORS.json "$SAMPLES"
