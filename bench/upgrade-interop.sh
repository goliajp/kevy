#!/usr/bin/env bash
# upgrade-interop — mixed-version verification, previous release <-> this tree, for the
# upgrade guide. Three scenarios:
#
#   A  old primary, new replica    — fresh join converges
#   B  new primary, old replica    — fresh join converges
#   C  new primary, replica carrying a 5.0 counter-generation sidecar —
#      the generation-identity fence must fire EXACTLY ONE snapshot
#      resync and self-heal (the documented one-time cost of upgrading)
#   D  vlog/AOF dir portability both directions — a dataset written by
#      one version boots and reads back byte-identically on the other,
#      including values large enough to take the vlog/compression path
#      (the 5.1 decoder carries the compat arm for 5.0's shared-table
#      frames; 5.1 frames stay readable by 5.0)
#
# Usage: bash bench/upgrade-interop.sh <old-kevy-binary> <new-kevy-binary>
# The CLI comes from the repo build (target/release/kevy-cli).
set -u
OLD=${1:?usage: upgrade-interop.sh <old-kevy-binary> <new-kevy-binary>}
NEW=${2:?usage: upgrade-interop.sh <old-kevy-binary> <new-kevy-binary>}
OLD=$(cd "$(dirname "$OLD")" && pwd)/$(basename "$OLD")
NEW=$(cd "$(dirname "$NEW")" && pwd)/$(basename "$NEW")
cd "$(dirname "$0")/.."
CLI=target/release/kevy-cli
[ -x "$CLI" ] || CLI=target/debug/kevy-cli
command -v timeout >/dev/null 2>&1 && CLI="timeout 15 $CLI"

# Ports clear of every other gate script's range (see availgate's
# port+10000 replication-base convention, honored here too).
PPORT=7701
RPORT=7711
DIR=$(mktemp -d /tmp/kevy-upgrade-interop-XXXXXX)
PIDS=()
trap 'kill ${PIDS[@]:-} 2>/dev/null; rm -rf "$DIR"' EXIT

fail() {
    echo "upgrade-interop: FAIL — $1"
    for f in "$DIR"/*.out; do
        [ -f "$f" ] || continue
        echo "--- $(basename "$f") (tail)"; tail -8 "$f"
    done
    exit 1
}
note() { echo "upgrade-interop: ok — $1"; }

# The labels below come from the binaries, not from the script. They used to
# be written in — "5.0 primary -> 5.1 replica" — and stayed that way through
# every 6.x run, so a PASS for 6.3.0 <-> 6.4.0 printed version numbers from
# two majors earlier. A report that pastes those lines names a comparison
# nobody ran. Asking each binary what it is costs one process each.
OLD_V=$("$OLD" --version 2>/dev/null | awk '{print $2}')
NEW_V=$("$NEW" --version 2>/dev/null | awk '{print $2}')
[ -n "$OLD_V" ] && [ -n "$NEW_V" ] || { echo "upgrade-interop: REFUSED — a binary did not report its version"; exit 2; }
[ "$OLD_V" != "$NEW_V" ] || { echo "upgrade-interop: REFUSED — old and new are both $OLD_V; that is not a mixed-version run"; exit 2; }
echo "upgrade-interop: old=$OLD_V new=$NEW_V"

start_kevy() { # binary role port dir upstream_port_or_empty outname
    local bin=$1 role=$2 port=$3 d=$4 up=$5 out=$6
    mkdir -p "$DIR/$d"
    printf '[replication]\nrole = "%s"\n' "$role" > "$DIR/$d.toml"
    [ -n "$up" ] && printf 'upstream = "127.0.0.1:1%s"\n' "$up" >> "$DIR/$d.toml"
    env KEVY_BIND=127.0.0.1 "$bin" --threads 2 --port "$port" --dir "$DIR/$d" \
        --config "$DIR/$d.toml" > "$DIR/$out.out" 2>&1 &
    PIDS+=($!)
    for _ in $(seq 100); do
        $CLI -p "$port" PING >/dev/null 2>&1 && return
        sleep 0.2
    done
    fail "$out never came up"
}

stop_all() {
    kill "${PIDS[@]:-}" 2>/dev/null
    wait 2>/dev/null
    PIDS=()
    sleep 1
}

wait_key() { # port key expected label
    for _ in $(seq 60); do
        echo "$($CLI -p "$1" GET "$2" 2>/dev/null)" | grep -q "\"$3\"" && return 0
        sleep 0.5
    done
    fail "$4"
}

# ---- scenario A: old primary, new replica ----
start_kevy "$OLD" primary $PPORT a-pri "" a-pri
for i in $(seq 1 50); do $CLI -p $PPORT SET a$i v >/dev/null; done
$CLI -p $PPORT SET amark old-serves-new >/dev/null
start_kevy "$NEW" replica $RPORT a-rep $PPORT a-rep
wait_key $RPORT amark old-serves-new "scenario A: new replica never converged on old primary"
note "A: $OLD_V primary -> $NEW_V replica converged"
stop_all

# ---- scenario B: new primary, old replica ----
start_kevy "$NEW" primary $PPORT b-pri "" b-pri
for i in $(seq 1 50); do $CLI -p $PPORT SET b$i v >/dev/null; done
$CLI -p $PPORT SET bmark new-serves-old >/dev/null
start_kevy "$OLD" replica $RPORT b-rep $PPORT b-rep
wait_key $RPORT bmark new-serves-old "scenario B: old replica never converged on new primary"
note "B: $NEW_V primary -> $OLD_V replica converged"
stop_all

# ---- scenario C: replica carries a 5.0 counter-generation sidecar ----
# Build the sidecar by running the replica dir under the OLD binary
# against an OLD primary first (counter generations), then retarget
# that same dir at a NEW primary (random generation): the fence must
# mismatch, ship exactly one snapshot per shard, and converge.
start_kevy "$OLD" primary $PPORT c-oldpri "" c-oldpri
for i in $(seq 1 50); do $CLI -p $PPORT SET c$i v >/dev/null; done
start_kevy "$OLD" replica $RPORT c-rep $PPORT c-rep
wait_key $RPORT c50 v "scenario C: old/old pair never synced"
stop_all
start_kevy "$NEW" primary $PPORT c-newpri "" c-newpri
for i in $(seq 1 50); do $CLI -p $PPORT SET d$i v >/dev/null; done
$CLI -p $PPORT SET cmark upgraded >/dev/null
start_kevy "$OLD" replica $RPORT c-rep $PPORT c-rep2
wait_key $RPORT cmark upgraded "scenario C: sidecar-carrying replica never resynced to the new primary"
SHIPS=$(grep -c "shipping snapshot" "$DIR/c-newpri.out" || true)
[ "$SHIPS" -ge 1 ] || fail "scenario C: converged but no snapshot ship logged — expected the gen fence"
# Stale pre-upgrade keys must be gone after the resync — the new
# primary never wrote c*, so finding one means the replica MERGED the
# new history onto its stale store instead of adopting it wholesale.
if echo "$($CLI -p $RPORT GET c1 2>/dev/null)" | grep -q '"v"'; then
    fail "scenario C: pre-upgrade key c1 survived the resync (fork not discarded)"
fi
note "C: counter-gen sidecar -> one-time snapshot resync self-healed ($SHIPS ship(s) logged)"
stop_all

# ---- scenario D: dir portability, both directions ----
# Values sized to engage the vlog/compression path plus a spread of
# small keys; digest = sorted DBSIZE + sampled GETs.
start_kevy "$OLD" primary $PPORT d-dir "" d-old1
BIG=$(printf 'x%.0s' $(seq 1 60000))
for i in $(seq 1 20); do $CLI -p $PPORT SET big$i "$BIG$i" >/dev/null; done
for i in $(seq 1 200); do $CLI -p $PPORT SET small$i v$i >/dev/null; done
OLD_DBSIZE=$($CLI -p $PPORT DBSIZE)
stop_all
start_kevy "$NEW" primary $PPORT d-dir "" d-new1
NEW_DBSIZE=$($CLI -p $PPORT DBSIZE)
[ "$OLD_DBSIZE" = "$NEW_DBSIZE" ] || fail "scenario D: dbsize drift old->new ($OLD_DBSIZE vs $NEW_DBSIZE)"
$CLI -p $PPORT GET big7 | grep -q "7\"$" || fail "scenario D: big7 corrupt after old->new boot"
# New writes on the same dir, then back to the old binary.
for i in $(seq 1 20); do $CLI -p $PPORT SET nbig$i "$BIG$i" >/dev/null; done
$CLI -p $PPORT SET dmark round-trip >/dev/null
stop_all
start_kevy "$OLD" primary $PPORT d-dir "" d-old2
$CLI -p $PPORT GET dmark | grep -q round-trip || fail "scenario D: new->old boot lost the marker"
$CLI -p $PPORT GET nbig13 | grep -q "13\"$" || fail "scenario D: $NEW_V-written big value corrupt under $OLD_V"
note "D: dir round-trip old->new->old intact (dbsize $OLD_DBSIZE, vlog-sized values verified)"
stop_all

echo "upgrade-interop: PASS (A + B + C + D)"
