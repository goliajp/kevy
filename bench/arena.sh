#!/usr/bin/env bash
# arena — kevy against valkey, Redis and Dragonfly, one engine on the cores at
# a time, two tables per run: throughput and cost per op.
#
#   - topology: the engine gets 4 cores on CPUs 0-3 (4 threads / io-threads /
#     proactor threads), their SMT siblings 8-11 stay empty, the load gets both
#     threads of cores 4-7 (8 threads). The old 0-7 / 8-15 split paired every
#     server thread with a load thread on one physical core;
#   - throughput is read from the SERVER's command counter over a wall window
#     timed here, NOT from redis-benchmark's rate: under `--threads` the
#     benchmark exits on its own 250ms showThroughput timer
#     (redis-benchmark.c:52, :1653; without --threads it stops in clientDone
#     at :425), so its rate is quantized to N/(k*250ms) and understated. Both
#     engines expose the same counter, so the comparison stays like-for-like;
#   - cost per op in the same window: instructions (user and kernel apart, so
#     the kernel's per-syscall tax on this box is visible as such), cycles,
#     syscalls, and the engine's CPU use, from `perf stat -p` on the engine's
#     host pid. The cost table holds on a busy box; the throughput table does
#     not;
#   - every window is checked like perfgate2's: a busy sibling core, foreign
#     load on the engine's cores or a load generator near its CPU limit
#     discards it; a window with more than 10% foreign load on the box marks
#     the round dirty (exit 3), and arena-median retakes dirty rounds;
#   - after a cell's windows, one more window with 16 load threads: if it
#     moves more than 2%, the load generator was the limit and the cell is
#     CLIENT-BOUND, not a result;
#   - competitor versions and image digests, and the box's own settings, are
#     in the output header.
#
# Usage (lx64): bash bench/arena.sh <kevy-binary>
# ARENA_SAMPLES=<file> keeps every window as JSONL (arena-median reads it).
set -u
. "$(dirname "$0")/bench-lock.sh"   # hold the machine's bench lock for the whole run
# ROOT: arena is the one documented exception to "bench scripts do not run
# as root" (hard rule 4). It
# needs docker to run the competitors, docker on the bench box is root-only,
# and rootless cannot substitute: `--cpuset-cpus` requires the cpuset
# controller to be delegated to the user slice, and it is not (user slices
# get cpu/memory/pids only), so a rootless run would silently lose the core
# pinning the whole fair-fight protocol rests on. An unpinned number is
# worse than no number.
#
# The exception is bounded by construction, which is what the rule is
# actually protecting: arena never calls pkill — it kills the PID it
# spawned and removes the containers it named — and none of its `docker
# run` invocations mount a host path. perfgate keeps its hard root refusal,
# because perfgate does use `pkill -f`. Being root is also what lets arena
# attach perf to the containerised engines.

KBIN=${1:?usage: arena.sh <kevy-binary>}
KBIN=$(cd "$(dirname "$KBIN")" && pwd)/$(basename "$KBIN")
cd "$(dirname "$0")"

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
MAX_DISCARDS=6
PORT=7201
TESTS="get set incr lpush sadd hset zadd"
TOPO=$PWD/PERF-BASELINE2.json
SAMPLES=${ARENA_SAMPLES:-$(mktemp)}
[ -n "${ARENA_SAMPLES:-}" ] || trap 'rm -f "$SAMPLES"' EXIT
: >"$SAMPLES"

# Competitor versions come from bench/COMPETITOR-ANCHORS.json; each image is
# asked what it actually is, and a mismatch stops the run.
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

. ./arena-box.sh

echo "# arena — $(date -u +%F) — $($KBIN --version | head -1)"
echo "# engines: $ENGINES"
echo "# protocol: -c $CONC -P $PIPE, engine cpus $SRV_CORES ($SRV_THREADS threads), cpus 8-11 idle, load cpus $CLI_CORES ($CLI_THREADS threads), $RUNS windows of ${WINDOW}s after a ${RAMP}s ramp"
echo "# measured: server-side total_commands_processed and perf stat -p on the engine, same window (NOT redis-benchmark's rate — see the header)"
box_facts
echo "# note: the redis-benchmark -t cells use one fixed key per command type"

echo "server test median stdev"

run_server_and_measure kevy \
    taskset -c "$SRV_CORES" env KEVY_BIND=127.0.0.1 "$KBIN" --threads "$SRV_THREADS" --port $PORT --no-aof

run_server_and_measure valkey \
    docker run --rm --name arena-valkey --network host --cpuset-cpus "$SRV_CORES" \
    "valkey/valkey:$VALKEY_PIN" valkey-server --port $PORT --save '' --appendonly no --io-threads "$SRV_THREADS"

# Measuring all four through one harness is the only way the position plot
# means anything. Set FOURWAY=0 to run the bare kevy-vs-valkey table only.
if [ "${FOURWAY:-1}" = 1 ]; then
    run_server_and_measure redis8 \
        docker run --rm --name arena-redis8 --network host --cpuset-cpus "$SRV_CORES" \
        "redis:$REDIS_PIN" redis-server --port $PORT --save '' --appendonly no --io-threads "$SRV_THREADS"

    run_server_and_measure dragonfly \
        docker run --rm --name arena-dragonfly --network host --cpuset-cpus "$SRV_CORES" \
        --ulimit memlock=-1 "docker.dragonflydb.io/dragonflydb/dragonfly:v$DRAGONFLY_PIN" \
        --port $PORT --proactor_threads="$SRV_THREADS"
fi

python3 arena_table.py run "$TOPO" "$SAMPLES"
