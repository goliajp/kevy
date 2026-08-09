#!/usr/bin/env bash
# upgradegate — the L6 release gate (charter §3): swapping the binary is
# the whole upgrade story, in BOTH directions, plus backup-is-a-file-copy
# and the zero-config boot path.
#
#   bash bench/upgradegate.sh [OLD_BIN] [NEW_BIN]
#
# OLD_BIN defaults to a cargo-installed kevy 4.1.1 (the last published
# release); NEW_BIN to target/release/kevy. The sequence:
#
#   A. OLD serves a directory, writes one value of every persisted shape
#      (string, big ArcBulk string, hash, list, set, zset, TTLs).
#   B. NEW opens the same directory — every byte OLD wrote must read
#      back — then writes its own generation on top.
#   C. OLD opens it again: the DOWNGRADE direction. Both files speak
#      AOF v2, so a rollback deploy must see both generations.
#   D. Backup = cp -r of the directory; NEW serves the copy identically.
#   E. Zero-config: NEW boots with nothing but --port/--dir and answers.
#
# Exit 0 = PASS. Any snapshot mismatch prints the diff and fails.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OLD_BIN=${1:-/tmp/upg-cache-4.1.1/bin/kevy}
NEW_BIN=${2:-"$HERE/../target/release/kevy"}
PORT=${UPGRADEGATE_PORT:-6301}
CLI="redis-cli -p $PORT"
DIR=$(mktemp -d "${TMPDIR:-/tmp}/upgradegate-XXXXXX")
SRV_PID=""

fail() { echo "upgradegate: FAIL — $1" >&2; exit 1; }
command -v redis-cli >/dev/null 2>&1 || { echo "upgradegate: REFUSED — redis-cli not found (the gate drives both binaries through it)"; exit 2; }
[ -x "$OLD_BIN" ] || fail "old binary missing at $OLD_BIN (cargo install kevy --version 4.1.1 --root /tmp/upg-cache-4.1.1)"
[ -x "$NEW_BIN" ] || fail "new binary missing at $NEW_BIN (cargo build --release -p kevy)"

cleanup() {
    [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$DIR"
}
trap cleanup EXIT

start() { # $1 = binary, $2 = dir
    "$1" --port "$PORT" --dir "$2" --appendfsync always >"$DIR/srv.log" 2>&1 &
    SRV_PID=$!
    for _ in $(seq 1 100); do
        $CLI ping >/dev/null 2>&1 && return 0
        kill -0 "$SRV_PID" 2>/dev/null || { cat "$DIR/srv.log" >&2; fail "server died at boot"; }
        sleep 0.1
    done
    fail "server not ready"
}
stop() {
    $CLI shutdown nosave >/dev/null 2>&1 || true
    wait "$SRV_PID" 2>/dev/null || true
    SRV_PID=""
}

write_generation() { # $1 = tag
    local tag=$1 big
    big=$(printf 'v%.0s' $(seq 1 200)) # >64 B — the ArcBulk shape
    $CLI set "str:$tag" "plain-$tag" >/dev/null
    $CLI set "big:$tag" "$big$tag" >/dev/null
    $CLI hset "hash:$tag" f1 "a-$tag" f2 "b-$tag" long_field_name_over_22b "c-$tag" >/dev/null
    $CLI rpush "list:$tag" "one-$tag" "two-$tag" "three-$tag" >/dev/null
    $CLI sadd "set:$tag" "m1-$tag" "m2-$tag" >/dev/null
    $CLI zadd "zset:$tag" 1.5 "low-$tag" 2.5 "high-$tag" >/dev/null
    $CLI set "ttl:$tag" "expiring-$tag" ex 3600 >/dev/null
}

snapshot() { # stdout: a canonical text image of every generation present
    local tag
    for tag in gen-old gen-new; do
        $CLI exists "str:$tag" | grep -q 1 || continue
        echo "== $tag =="
        echo "str: $($CLI get "str:$tag")"
        echo "big: $($CLI strlen "big:$tag")"
        echo "hash: $($CLI hgetall "hash:$tag" | paste -sd, -)"
        echo "list: $($CLI lrange "list:$tag" 0 -1 | paste -sd, -)"
        echo "set: $($CLI smembers "set:$tag" | sort | paste -sd, -)"
        echo "zset: $($CLI zrange "zset:$tag" 0 -1 WITHSCORES | paste -sd, -)"
        local ttl
        ttl=$($CLI ttl "ttl:$tag")
        [ "$ttl" -gt 0 ] && [ "$ttl" -le 3600 ] && echo "ttl: live" || echo "ttl: BAD($ttl)"
    done
}

check() { # $1 = expected snapshot file, $2 = phase name
    snapshot >"$DIR/got.txt"
    diff -u "$1" "$DIR/got.txt" >&2 || fail "$2: the reopened directory disagrees"
    echo "  ✓ $2"
}

echo "upgradegate: old=$("$OLD_BIN" --version 2>/dev/null | head -1 || echo "$OLD_BIN")"
echo "upgradegate: new=$("$NEW_BIN" --version 2>/dev/null | head -1 || echo "$NEW_BIN")"

# A — the last release writes its world.
start "$OLD_BIN" "$DIR/data"
write_generation gen-old
snapshot >"$DIR/snap-a.txt"
stop

# B — the candidate opens it (UPGRADE), then writes its own generation.
start "$NEW_BIN" "$DIR/data"
check "$DIR/snap-a.txt" "upgrade: new binary serves the old generation"
write_generation gen-new
snapshot >"$DIR/snap-ab.txt"
stop

# C — the last release opens BOTH generations (DOWNGRADE).
start "$OLD_BIN" "$DIR/data"
check "$DIR/snap-ab.txt" "downgrade: old binary serves both generations"
stop

# D — backup is a file copy.
cp -R "$DIR/data" "$DIR/backup"
start "$NEW_BIN" "$DIR/backup"
check "$DIR/snap-ab.txt" "backup: a copied directory serves identically"
stop

# E — zero-config boot.
start "$NEW_BIN" "$DIR/fresh"
$CLI ping | grep -q PONG || fail "zero-config boot did not answer PING"
echo "  ✓ zero-config: bare --port/--dir boots and answers"
stop

echo "upgradegate: PASS (upgrade + downgrade + backup-copy + zero-config)"
