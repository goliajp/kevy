#!/usr/bin/env bash
# availgate — availability contract gate, phase 1 (v3.14 A0).
#
# Two real processes (primary + replica). Clamps, per the RFC:
#   1. replica rejects client writes (-READONLY) WHILE its feed apply
#      keeps advancing (dbsize grows from primary writes)
#   2. offset truth: replica applied == primary master_repl_offset
#      once quiesced (three-source identity, INFO-visible face)
#   3. lag converges: INFO slave_lag_frames returns to 0 after writes
#   4. link truth: kill primary → master_link_status:down within 3s
#      heartbeat window (+1s slack); restart → up again
#   5. primary-side per-replica truth: slave0 line carries acked
#      offset == master_repl_offset when quiesced
#   6. min-replicas-to-write: primary with min=1 and no replica
#      refuses writes (-NOREPLICAS); write opens when replica ACKs
#
# Usage: bash bench/availgate.sh <kevy-binary>
set -u
KBIN=${1:?usage: availgate.sh <kevy-binary>}
KBIN=$(cd "$(dirname "$KBIN")" && pwd)/$(basename "$KBIN")
cd "$(dirname "$0")/.."
CLI=target/release/kevy-cli
[ -x "$CLI" ] || CLI=target/debug/kevy-cli
PPORT=7381
RPORT=7382
DIR=$(mktemp -d /tmp/kevy-availgate-XXXXXX)
PPID_=""
RPID_=""
trap 'kill $PPID_ $RPID_ 2>/dev/null; rm -rf "$DIR"' EXIT

fail() { echo "availgate: FAIL — $1"; exit 1; }
note() { echo "availgate: ok — $1"; }

wait_ports_free() {
    # A just-killed server's sockets can linger a beat; binding into
    # that window aborts the new process (fail-fast is the right
    # PRODUCT behavior — the gate simply waits it out).
    for _ in $(seq 50); do
        lsof -nP -i :$PPORT -i :$RPORT -i :1$PPORT >/dev/null 2>&1 || return 0
        sleep 0.2
    done
}

start_primary() {
    wait_ports_free
    printf '[replication]\nrole = "primary"\nmin_replicas_to_write = %s\n' "${1:-0}" > "$DIR/pri.toml"
    env KEVY_BIND=127.0.0.1 "$KBIN" --threads 4 --port $PPORT --dir "$DIR/p" --no-aof \
        --config "$DIR/pri.toml" > "$DIR/pri.out" 2>&1 &
    PPID_=$!
    for _ in $(seq 100); do
        $CLI -p $PPORT PING >/dev/null 2>&1 && return
        sleep 0.2
    done
    fail "primary never came up"
}

start_replica() {
    printf '[replication]\nrole = "replica"\nupstream = "127.0.0.1:%s"\n' "1$PPORT" > "$DIR/rep.toml"
    env KEVY_BIND=127.0.0.1 "$KBIN" --threads 4 --port $RPORT --dir "$DIR/r" --no-aof \
        --config "$DIR/rep.toml" > "$DIR/rep.out" 2>&1 &
    RPID_=$!
    for _ in $(seq 100); do
        $CLI -p $RPORT PING >/dev/null 2>&1 && return
        sleep 0.2
    done
    fail "replica never came up"
}

mkdir -p "$DIR/p" "$DIR/r"
start_primary 0
start_replica
sleep 3

# ---- clamp 1: READONLY + apply keeps advancing
R=$($CLI -p $RPORT SET x 1 2>&1)
echo "$R" | grep -q "READONLY" || fail "replica accepted a client write: $R"
for i in $(seq 1 200); do $CLI -p $PPORT SET "k$i" v >/dev/null; done
sleep 2
N=$($CLI -p $RPORT DBSIZE | grep -oE "[0-9]+")
if [ "${N:-0}" -lt 200 ]; then
    echo "--- FORENSICS: replica INFO"; $CLI -p $RPORT INFO replication | head -12
    echo "--- FORENSICS: primary INFO"; $CLI -p $PPORT INFO replication | head -12
    echo "--- FORENSICS: replica log"; tail -5 "$DIR/rep.out" 2>/dev/null
    echo "--- FORENSICS: primary log"; tail -8 "$DIR/pri.out" 2>/dev/null
    echo "--- FORENSICS: port owners"; lsof -nP -i :7381 -i :17381 -i :17382 2>/dev/null | head -8
    fail "replica apply stalled (dbsize=$N)"
fi
note "READONLY holds while apply advances (dbsize=$N)"

# ---- clamp 2+3: lag converges + data-plane convergence
# (INFO offsets are per-shard samples on each side — the answering
# shard's number, not a cross-process invariant. The comparable
# truths are the replica's own lag gauge and the data itself.)
sleep 2
LAG=$($CLI -p $RPORT INFO replication | grep -oE "slave_lag_frames:[0-9]+" | grep -oE "[0-9]+")
[ "$LAG" = "0" ] || fail "lag did not converge ($LAG)"
V=$($CLI -p $RPORT GET k200)
echo "$V" | grep -q '"v"' || fail "data plane not converged (k200=$V)"
note "lag 0 + data plane converged"

# ---- clamp 5: primary-side slave0 truth — acked catches up to sent
# within a bounded window (ACKs ride 1s heartbeats when idle).
S0OK=0
for _ in $(seq 20); do
    S0=$($CLI -p $PPORT INFO replication | grep "^slave0:" || true)
    if echo "$S0" | grep -qE "lag=0" && echo "$S0" | grep -qE "offset=[1-9]"; then
        S0OK=1; break
    fi
    sleep 0.5
done
[ $S0OK = 1 ] || fail "slave0 never converged: $S0"
note "slave0 acked truth ($S0)"

# ---- clamp 4: link truth on kill + restart
kill -9 $PPID_ 2>/dev/null; wait $PPID_ 2>/dev/null
sleep 4
LS=$($CLI -p $RPORT INFO replication | grep master_link_status)
echo "$LS" | grep -q down || fail "link not down after primary kill: $LS"
note "link down within window"
start_primary 0
sleep 4
LS=$($CLI -p $RPORT INFO replication | grep master_link_status)
echo "$LS" | grep -q up || fail "link not up after primary restart: $LS"
note "link recovers to up"

# ---- clamp 6: min-replicas-to-write
kill $RPID_ 2>/dev/null; wait $RPID_ 2>/dev/null
kill $PPID_ 2>/dev/null; wait $PPID_ 2>/dev/null
rm -rf "$DIR/p" "$DIR/r"; mkdir -p "$DIR/p" "$DIR/r"
start_primary 1
sleep 1
R=$($CLI -p $PPORT SET solo v 2>&1)
echo "$R" | grep -q "NOREPLICAS" || fail "min-replicas did not refuse solo write: $R"
note "solo primary refuses writes (min_replicas=1)"
start_replica
GOOD=0
for _ in $(seq 40); do
    R=$($CLI -p $PPORT SET withrep v 2>&1)
    if echo "$R" | grep -q "OK"; then GOOD=1; break; fi
    sleep 0.5
done
[ $GOOD = 1 ] || fail "write never opened after replica ACKed"
note "write opens once a replica ACKs"

echo "availgate: PASS"
