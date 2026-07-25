#!/usr/bin/env bash
# profile-textgate — a symbol-resolved CPU profile of textgate's query phase.
#
# The perf-vs-foss rule requires a profile before an attack, and getting a
# usable one here took four tries. Each failure produced plausible numbers,
# which is why they are all encoded below rather than left as folklore:
#
#   1. `[profile.release]` sets `strip = true`, so a release binary resolves
#      nothing but libc — our own frames come back bare addresses. Build
#      `--profile profiling` (same codegen, symbols kept).
#   2. `perf record -p PID -- sleep N` does NOT bound the recording: perf
#      kept sampling until the gate's teardown SIGTERMed it (exit 143).
#      `timeout -s INT` stops it on our clock and lets it finalise.
#   3. `pgrep -f /tmp/<name>` also matches the `su` and the gate's own bash,
#      both of which carry the binary path in argv and hold LOWER pids than
#      the server. Match the server's argv and prove it before attaching.
#   4. The gate's python writes to a file, so its stdout is block-buffered
#      and the "text index built" marker arrived at flush time — i.e. at
#      shutdown. Every window opened on the shard teardown instead of the
#      queries, and a teardown that frees a million positional blobs buries
#      everything else under libc. PYTHONUNBUFFERED=1 fixes the clock.
#
# The check at the end is the one that would have caught all four: if any
# `drop_glue` frame appears, the window is sampling teardown, not queries.
#
#   bash bench/profile-textgate.sh [name] [mode-env...]
#   POSITIONS=1 bash bench/profile-textgate.sh phrase
#
# perf_event_paranoid is 3 on the shared bench box, so recording needs root
# while the measured server stays unprivileged: run this as root, it drops
# to kevybench for the build and the gate and keeps only the sampler.
set -uo pipefail
NAME=${1:-kevy-prof}
WINDOW=${WINDOW:-6}
USER_ACCT=${USER_ACCT:-kevybench}
MODE="POSITIONS=${POSITIONS:-0} PREFIX=${PREFIX:-0} FIELDS=${FIELDS:-0} VALUES=${VALUES:-0} ORDER=${ORDER:-0} TYPO=${TYPO:-0}"

su - "$USER_ACCT" -c "cd ~/kevy && cargo build -q --profile profiling -p kevy && cp target/profiling/kevy /tmp/$NAME" || exit 1

OUT=$(mktemp "/tmp/${NAME}-gate-XXXXXX")
chmod 666 "$OUT"
su - "$USER_ACCT" -c "cd ~/kevy && PYTHONUNBUFFERED=1 $MODE bash bench/textgate.sh /tmp/$NAME" >"$OUT" 2>&1 &
GATE=$!

for _ in $(seq 1 900); do grep -q 'text index built' "$OUT" 2>/dev/null && break; sleep 1; done
PID=$(pgrep -f "$NAME --threads" | head -1)
if [ -z "$PID" ] || ! tr '\0' ' ' < "/proc/$PID/cmdline" 2>/dev/null | grep -q -- '--threads'; then
    echo "profile-textgate: FAIL — no server process to attach to"
    wait "$GATE"; tail -5 "$OUT"; exit 1
fi
echo "server pid=$PID: $(tr '\0' ' ' < "/proc/$PID/cmdline" | cut -c1-70)"

DATA=$(mktemp "/root/${NAME}-XXXXXX.data")
timeout -s INT "$WINDOW" perf record -F 499 --call-graph dwarf,16384 -p "$PID" -o "$DATA"
wait "$GATE"
grep -E 'p95|PASS|FAIL' "$OUT"

echo "=== top self-time symbols ==="
perf report -i "$DATA" --no-children -g none --stdio --percent-limit 1.0 2>/dev/null \
    | grep -E '^ +[0-9]' | head -14
# Teardown check, by weight rather than by presence: a shard being dropped
# shows up as tens of percent (freeing a million positional blobs is not
# subtle), while a stray sub-1% frame is just the tail of the previous
# phase and does not invalidate the profile. Any-occurrence was too strict
# and would have failed a usable TYPO profile over one 0.x% frame.
drop_pct=$(perf report -i "$DATA" --no-children -g none --stdio 2>/dev/null \
    | awk '/drop_glue/ && $1 ~ /%$/ { gsub("%","",$1); s += $1 } END { printf "%.1f", s+0 }')
if awk "BEGIN{exit !($drop_pct >= 2.0)}"; then
    echo "profile-textgate: SUSPECT — ${drop_pct}% of samples are drop_glue: the"
    echo "  window is sampling shard teardown, not queries. Shorten WINDOW or"
    echo "  check that the build marker is arriving unbuffered."
    exit 1
fi
echo "profile-textgate: clean — teardown frames ${drop_pct}% of samples (want <2%)"
