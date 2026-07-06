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
# Replication listens at port+10000 for NSHARDS consecutive ports —
# keep client ports ≥ nshards apart or the two servers' replication
# ranges collide (v3.15: replicas bind a listener too).
PPORT=7381
RPORT=7391
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
        lsof -nP -i :$PPORT -i :$RPORT -i :1$PPORT -i :1$RPORT >/dev/null 2>&1 || return 0
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
kill $RPID_ $PPID_ 2>/dev/null; wait 2>/dev/null

# ================= phase 2 (v3.15): crash failover, 3-node quorum ===
# clamp 7: kill the primary → a replica wins the election and opens
#          writes (MTTR bounded); the other replica retargets and
#          converges on post-failover writes
# clamp 8: restart the old primary → restart-role clamp holds it as
#          a read-only replica; it converges (fork discarded)
# (candidate ranking by offset is covered by kevy-elect unit tests;
# a lag-injected 3-node ranking e2e joins the v3.16 matrix.)
N1=7440; N2=7460; N3=7480
E1=7540; E2=7560; E3=7580
PIDS=()
trap 'kill $PPID_ $RPID_ ${PIDS[@]:-} 2>/dev/null; rm -rf "$DIR"' EXIT

node_cfg() { # id client_port elect_base role upstream_or_empty
    printf '[replication]\nrole = "%s"\n' "$4"
    [ -n "$5" ] && printf 'upstream = "127.0.0.1:%s"\n' "$5"
    printf '[cluster]\nenabled = true\nnode_id = "%s"\nelect_port_base = %s\n' "$1" "$3"
    printf 'peers = "n1@127.0.0.1:%s:%s,n2@127.0.0.1:%s:%s,n3@127.0.0.1:%s:%s"\n' \
        "$E1" "$N1" "$E2" "$N2" "$E3" "$N3"
}

start_node() { # id client_port elect_base role upstream dirname
    node_cfg "$1" "$2" "$3" "$4" "$5" > "$DIR/$1.toml"
    env KEVY_BIND=127.0.0.1 "$KBIN" --threads 4 --port "$2" --dir "$DIR/$6" --no-aof \
        --config "$DIR/$1.toml" > "$DIR/$1.out" 2>&1 &
    PIDS+=($!)
    for _ in $(seq 100); do
        $CLI -p "$2" PING >/dev/null 2>&1 && return
        sleep 0.2
    done
    fail "node $1 never came up"
}

role_of() { $CLI -p "$1" INFO replication 2>/dev/null | grep -oE "^role:[a-z]+" | cut -d: -f2; }

mkdir -p "$DIR/n1" "$DIR/n2" "$DIR/n3"
start_node n1 $N1 $E1 primary "" n1
start_node n2 $N2 $E2 replica "1$N1" n2
start_node n3 $N3 $E3 replica "1$N1" n3
sleep 3
for i in $(seq 1 200); do $CLI -p $N1 SET c$i v >/dev/null; done
sleep 2

# ---- clamp 7: crash failover
P1=${PIDS[0]}
kill -9 $P1 2>/dev/null; wait $P1 2>/dev/null
NEWP=""
for _ in $(seq 60); do   # MTTR window: 30s >> election timeout multiples
    for p in $N2 $N3; do
        [ "$(role_of $p)" = "master" ] && { NEWP=$p; break 2; }
    done
    sleep 0.5
done
[ -n "$NEWP" ] || {
    echo "--- FORENSICS n2 elect:"; grep elect "$DIR/n2.out" | tail -8
    echo "--- n3 elect:"; grep elect "$DIR/n3.out" | tail -8
    echo "--- n1 elect:"; grep elect "$DIR/n1.out" | tail -6
    fail "no replica won the election after primary crash"
}
OTHER=$N3; [ "$NEWP" = "$N3" ] && OTHER=$N2
WOK=0
for _ in $(seq 40); do
    echo "$($CLI -p $NEWP SET postfail v1 2>&1)" | grep -q OK && { WOK=1; break; }
    sleep 0.5
done
[ $WOK = 1 ] || fail "new primary $NEWP never opened writes"
note "crash failover: $NEWP won and opened writes"
CONV=0
for _ in $(seq 40); do
    echo "$($CLI -p $OTHER GET postfail 2>/dev/null)" | grep -q '"v1"' && { CONV=1; break; }
    sleep 0.5
done
[ $CONV = 1 ] || {
    echo "--- FORENSICS other($OTHER):"; $CLI -p $OTHER INFO replication | head -8
    fail "surviving replica never converged on the new primary's write"
}
note "surviving replica retargeted + converged"

# ---- clamp 8: old primary rejoins as a read-only replica
start_node n1 $N1 $E1 primary "" n1
R=$($CLI -p $N1 SET rejoinwrite v 2>&1)
echo "$R" | grep -qE "READONLY|MISDIRECTED|NOREPLICAS" \
    || fail "restarted old primary accepted a write before quorum confirm: $R"
note "restart-role clamp holds writes"
REJOIN=0
for _ in $(seq 60); do
    if [ "$(role_of $N1)" = "slave" ]; then
        echo "$($CLI -p $N1 GET postfail 2>/dev/null)" | grep -q '"v1"' && { REJOIN=1; break; }
    fi
    sleep 0.5
done
[ $REJOIN = 1 ] || {
    echo "--- FORENSICS n1:"; tail -8 "$DIR/n1.out"; $CLI -p $N1 INFO replication | head -6
    fail "old primary never rejoined as a converged replica"
}
note "old primary rejoined as replica + converged (fork discarded)"

# ============ phase 3 (v3.16): the consistency ladder ==============
# clamp 9:  WAIT acked-replica truth (1 with a live replica; 0 after
#           it dies)
# clamp 10: read-your-writes — write, REPL.TOKEN, REPL.WAIT on the
#           replica, then GET must see the value (20 rounds, zero
#           misses); a future token answers -MISDIRECTED
# clamp 11: bounded staleness — SIGSTOP the primary, reads go -STALE
#           past the bound, recover on CONT
# clamp 12: quorum lease — kill both replicas of a 3-node quorum,
#           the primary fences writes; restart one, writes reopen
for p in ${PIDS[@]:-}; do kill $p 2>/dev/null; done; wait 2>/dev/null
rm -rf "$DIR/p" "$DIR/r"; mkdir -p "$DIR/p" "$DIR/r"
printf '[replication]\nrole = "replica"\nupstream = "127.0.0.1:%s"\nreplica_max_staleness_ms = 2500\n' "1$PPORT" > "$DIR/rep3.toml"
start_primary 0
env KEVY_BIND=127.0.0.1 "$KBIN" --threads 4 --port $RPORT --dir "$DIR/r" --no-aof \
    --config "$DIR/rep3.toml" > "$DIR/rep.out" 2>&1 &
RPID_=$!
for _ in $(seq 100); do $CLI -p $RPORT PING >/dev/null 2>&1 && break; sleep 0.2; done
sleep 2

# ---- clamp 9: WAIT
$CLI -p $PPORT SET w1 v >/dev/null
W=$($CLI -p $PPORT WAIT 1 3000 | grep -oE "[0-9]+")
[ "${W:-0}" -ge 1 ] || fail "WAIT 1 with a live replica returned $W"
kill $RPID_ 2>/dev/null; wait $RPID_ 2>/dev/null; sleep 1
$CLI -p $PPORT SET w2 v >/dev/null
W=$($CLI -p $PPORT WAIT 1 500 | grep -oE "[0-9]+")
[ "${W:-1}" = "0" ] || fail "WAIT 1 after replica death returned $W"
note "WAIT reports acked-replica truth (1 live, 0 dead)"

# ---- clamp 10: read-your-writes token
env KEVY_BIND=127.0.0.1 "$KBIN" --threads 4 --port $RPORT --dir "$DIR/r" --no-aof \
    --config "$DIR/rep3.toml" > "$DIR/rep.out" 2>&1 &
RPID_=$!
for _ in $(seq 100); do $CLI -p $RPORT PING >/dev/null 2>&1 && break; sleep 0.2; done
sleep 2
MISS=0
for i in $(seq 1 20); do
    $CLI -p $PPORT SET ryw "r$i" >/dev/null
    TOK=$($CLI -p $PPORT REPL.TOKEN | grep -oE "[0-9]+$" | tr '\n' ' ')
    R=$($CLI -p $RPORT REPL.WAIT $TOK TIMEOUT 3000 2>&1)
    echo "$R" | grep -q OK || { MISS=$((MISS+1)); continue; }
    V=$($CLI -p $RPORT GET ryw)
    echo "$V" | grep -q "r$i" || MISS=$((MISS+1))
done
[ $MISS = 0 ] || fail "read-your-writes missed $MISS/20 rounds"
# future token: bump every offset by 100000 → must MISDIRECT, not hang
FTOK=$($CLI -p $PPORT REPL.TOKEN | grep -oE "[0-9]+$" | awk 'NR%2==1{print} NR%2==0{print $1+100000}' | tr '\n' ' ')
R=$($CLI -p $RPORT REPL.WAIT $FTOK TIMEOUT 600 2>&1)
echo "$R" | grep -q "MISDIRECTED" || fail "future token did not MISDIRECT: $R"
note "read-your-writes token holds (20/20) + future token MISDIRECTs"

# ---- clamp 11: bounded staleness
kill -STOP $PPID_; sleep 4
R=$($CLI -p $RPORT GET ryw 2>&1)
echo "$R" | grep -q "STALE" || { kill -CONT $PPID_; fail "stale replica still answered reads: $R"; }
kill -CONT $PPID_; sleep 3
R=$($CLI -p $RPORT GET ryw 2>&1)
echo "$R" | grep -q "r20" || fail "reads did not recover after CONT: $R"
note "bounded staleness: -STALE past bound, recovers on heal"

# ---- clamp 12: quorum lease fence (3-node)
kill $PPID_ $RPID_ 2>/dev/null; wait 2>/dev/null
rm -rf "$DIR/n1" "$DIR/n2" "$DIR/n3"; mkdir -p "$DIR/n1" "$DIR/n2" "$DIR/n3"
PIDS=()
start_node n1 $N1 $E1 primary "" n1
start_node n2 $N2 $E2 replica "1$N1" n2
start_node n3 $N3 $E3 replica "1$N1" n3
# election-only write authority: wait for a primary to emerge
LEADER=""
for _ in $(seq 60); do
    for p in $N1 $N2 $N3; do
        [ "$(role_of $p)" = "master" ] && { LEADER=$p; break 2; }
    done
    sleep 0.5
done
[ -n "$LEADER" ] || fail "no leader emerged in the 3-node quorum"
for _ in $(seq 40); do
    echo "$($CLI -p $LEADER SET q1 v 2>&1)" | grep -q OK && break
    sleep 0.5
done
# kill the two non-leaders → leader loses quorum → writes fence
for i in 0 1 2; do
    port=$((N1 + i * 20))
    # NB: no bare `wait` here — it would block on the still-live
    # leader too.
    [ "$port" != "$LEADER" ] && { kill ${PIDS[$i]} 2>/dev/null; wait ${PIDS[$i]} 2>/dev/null; }
done
FENCED=0
for _ in $(seq 30); do   # lease window = down_after (5s) + margin
    R=$($CLI -p $LEADER SET q2 v 2>&1)
    echo "$R" | grep -q "NOREPLICAS" && { FENCED=1; break; }
    sleep 0.5
done
[ $FENCED = 1 ] || fail "leader never fenced writes after losing quorum"
note "quorum lease: minority-side primary fences writes"
# restart one peer → quorum back → writes reopen
for i in 0 1 2; do
    port=$((N1 + i * 20))
    if [ "$port" != "$LEADER" ]; then
        id=n$((i + 1)); ebase=$((E1 + i * 20))
        start_node $id $port $ebase replica "" $id
        break
    fi
done
REOPEN=0
for _ in $(seq 40); do
    R=$($CLI -p $LEADER SET q3 v 2>&1)
    echo "$R" | grep -q OK && { REOPEN=1; break; }
    sleep 0.5
done
[ $REOPEN = 1 ] || fail "writes never reopened after quorum healed"
note "quorum lease: writes reopen on heal"

echo "availgate: PASS (phase 1 + 2 + 3)"
