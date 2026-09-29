# shellcheck shell=bash
# Sourced by arena.sh: the box facts, and one engine's cells.

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

# One window through perfgate2's collector. $1 engine, $2 TEST, $3 run slot
# (0 = the headroom probe), $4 window, $5 pid:threads of the generator.
arena_window() {
    python3 perfgate2_window.py --baseline "$TOPO" --angle "$2" --side "$1" \
        --obs "$3" --win "$4" --srv-pid "$EPID" --srv-cpus "$SRV_CORES" \
        --port "$PORT" --secs "$WINDOW" --gens "$5" --perf direct
}

start_load() { # $1 test, $2 threads
    taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p "$PORT" \
        -t "$1" -n "$N" -c "$CONC" -P "$PIPE" --threads "$2" -q >/dev/null 2>&1 &
    BPID=$!
    sleep "$RAMP"
}

stop_load() { kill "$BPID" 2>/dev/null; wait "$BPID" 2>/dev/null; }

# RUNS kept windows of one cell, then the headroom probe. A discarded window
# is retaken; six in a row end the round as dirty, not as a number.
bench_cell() { # $1 engine, $2 test
    local verb kept=0 win=0 streak=0 out rc
    verb=$(echo "$2" | tr '[:lower:]' '[:upper:]')
    start_load "$2" "$CLI_THREADS"
    while [ "$kept" -lt "$RUNS" ]; do
        win=$((win + 1))
        out=$(arena_window "$1" "$verb" 1 "$win" "$BPID:$CLI_THREADS")
        rc=$?
        case $rc in
            0) echo "$out" >>"$SAMPLES"; kept=$((kept + 1)); streak=0 ;;
            10) streak=$((streak + 1))
                if [ "$streak" -ge "$MAX_DISCARDS" ]; then
                    stop_load
                    echo "!! $1 $verb: $MAX_DISCARDS windows in a row discarded — dirty round" >&2
                    return 3
                fi ;;
            *) stop_load; return 2 ;;
        esac
    done
    stop_load
    start_load "$2" "$PROBE_THREADS"
    # the probe's own window may count as client-bound: its number is the point
    out=$(arena_window "$1" "$verb" 0 1 "$BPID:$PROBE_THREADS")
    rc=$?
    stop_load
    [ "$rc" -eq 0 ] || [ "$rc" -eq 10 ] || return 2
    echo "$out" >>"$SAMPLES"
}

engine_pid() { # $1 label, $2 pid we spawned
    if [ "$1" = kevy ]; then echo "$2"
    else docker inspect -f '{{.State.Pid}}' "arena-$1"; fi
}

run_server_and_measure() { # label, start-command...
    local label=$1 spid t rc=0
    shift
    "$@" >/dev/null 2>&1 &
    spid=$!
    sleep 1
    if ! wait_ready; then
        # a missing engine is a hole in the table and must read as one
        echo "# !! $label never came up — its rows are ABSENT from this table" >&2
        echo "# !! $label ABSENT (did not start)"
        kill "$spid" 2>/dev/null
        docker rm -f "arena-$label" >/dev/null 2>&1 || true
        return 1
    fi
    EPID=$(engine_pid "$label" "$spid")
    for t in $TESTS; do
        bench_cell "$label" "$t" || { rc=$?; break; }
    done
    kill "$spid" 2>/dev/null
    wait "$spid" 2>/dev/null
    docker rm -f "arena-$label" >/dev/null 2>&1 || true
    [ "$rc" -eq 0 ] || exit "$rc"
    sleep 1
}
