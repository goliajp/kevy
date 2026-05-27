#!/usr/bin/env bash
# crash-test.sh — Wave 2 #4 power-loss simulation.
#
# Loops N rounds of:
#   1. start kevy
#   2. SET 100 keys
#   3. kill -9 (hard kill — drops in-flight buffered writes)
#   4. restart kevy
#   5. count how many keys survived
#
# Expected behaviour:
#   - appendfsync = always   ⇒ 100/100 survive every round (zero data loss)
#   - appendfsync = everysec ⇒ at least the keys SET more than 1 s ago
#     survive; the very last sub-second may be lost (matches Redis exactly)
#   - appendfsync = no       ⇒ surviving count is implementation-defined;
#     the AOF must still parse cleanly (no startup failure) — the
#     `aof_truncated_tail_is_tolerated_on_restart` integration test in
#     crates/kevy/tests/persistence.rs verifies the parse-clean invariant.
#
# This script is a manual / CI harness; the inline kevy-persist + kevy
# tests cover the truncation-tolerance invariant in `cargo test`.
#
# Usage:
#   bash bench/crash-test.sh [rounds=10] [appendfsync=everysec]

set -euo pipefail
cd "$(dirname "$0")/.."

ROUNDS=${1:-10}
APPENDFSYNC=${2:-everysec}

KEVY_BIN=${KEVY_BIN:-./target/release/kevy}
if [ ! -x "$KEVY_BIN" ]; then
    echo "building release kevy ..."
    cargo build --release -p kevy --bin kevy >&2
fi

# Need a CLI client. Prefer redis-cli (valkey-compat) if present; else fall
# back to a tiny bash-only RESP probe.
CLI=$(command -v redis-cli || command -v valkey-cli || true)
if [ -z "$CLI" ]; then
    echo "ERROR: redis-cli / valkey-cli not on PATH. Install one to run crash-test.sh." >&2
    exit 1
fi

DIR=$(mktemp -d -t kevy-crash-XXXXXX)
PORT=${PORT:-6190}
trap 'rm -rf "$DIR"; pkill -9 -P $$ 2>/dev/null || true' EXIT

write_config() {
    cat > "$DIR/kevy.toml" <<EOF
[server]
bind = "127.0.0.1"
port = $PORT
data_dir = "$DIR"

[persistence]
aof = true
appendfsync = "$APPENDFSYNC"
EOF
}

wait_ready() {
    for _ in $(seq 1 200); do
        if "$CLI" -p $PORT -h 127.0.0.1 PING 2>/dev/null | grep -q PONG; then
            return 0
        fi
        sleep 0.02
    done
    echo "kevy did not start" >&2
    return 1
}

write_config

echo "crash-test: $ROUNDS rounds, appendfsync=$APPENDFSYNC, dir=$DIR"
overall_loss=0
for round in $(seq 1 $ROUNDS); do
    "$KEVY_BIN" --config "$DIR/kevy.toml" &
    pid=$!
    wait_ready

    # Write 100 keys; round-tag them so each round's keys are distinct.
    for i in $(seq 1 100); do
        "$CLI" -p $PORT -h 127.0.0.1 SET "k$round-$i" "v" > /dev/null
    done
    # Give EverySec a chance to fsync (it ticks once per second).
    sleep 1.2

    # Hard kill — drops any buffered/unflushed bytes the process held.
    kill -9 $pid
    wait $pid 2>/dev/null || true

    # Restart
    "$KEVY_BIN" --config "$DIR/kevy.toml" &
    pid=$!
    wait_ready

    survived=0
    for i in $(seq 1 100); do
        val=$("$CLI" -p $PORT -h 127.0.0.1 GET "k$round-$i" 2>/dev/null || echo "")
        if [ "$val" = "v" ]; then
            survived=$((survived + 1))
        fi
    done
    loss=$((100 - survived))
    overall_loss=$((overall_loss + loss))

    kill $pid
    wait $pid 2>/dev/null || true

    printf "  round %2d: %3d/100 survived (lost %d)\n" "$round" "$survived" "$loss"
done

echo "TOTAL data lost across $ROUNDS rounds: $overall_loss key(s)"
if [ "$APPENDFSYNC" = "always" ] && [ "$overall_loss" -gt 0 ]; then
    echo "FAIL: appendfsync=always must lose zero keys"
    exit 1
fi
