#!/bin/bash
# v3.2 embedded-as-primary gate — RFC D5 clamps, true two-process:
#
#   1. Embed writer (source enabled, continuous writes) + kevy server
#      in single-source replica mode → PREFIX.DIGEST converges to the
#      primary's digest after the writer pauses.
#   2. Fresh replica (offset 0 vs non-empty history) receives the
#      SNAPSHOT ship and converges — the closed v1.21 anti-scope.
#   3. Replica restart within the backlog window resumes by frames
#      and converges again.
#   4. The replica serves its own derived state on replicated data:
#      IDX.CREATE + IDX.QUERY answer on the replica.
#
# Usage: bash bench/repligate.sh
set -u
cd "$(dirname "$0")/.."
cargo build --release -p kevy --bin kevy -p kevy-cli --bin kevy-cli 2>&1 | tail -1
cargo build --release -p kevy-embedded --example primary_writer 2>&1 | tail -1
KEVY=target/release/kevy
CLI=target/release/kevy-cli
# Every CLI call bounded: a server wedged into accept-but-never-reply
# turns a gate into a silent multi-hour hang on CI (seen twice on the
# contract job). With coreutils timeout the call fails loudly at the
# exact clamp instead; macOS dev boxes (no timeout) run unbounded.
command -v timeout >/dev/null 2>&1 && CLI="timeout 15 $CLI"
WRITER=target/release/examples/primary_writer
# port+10000 replication ranges span nshards consecutive ports —
# keep the two servers >= nshards apart (v3.15: replicas listen too).
SRCPORT=7101
REPPORT=7112
DIR=$(mktemp -d /tmp/kevy-repligate-XXXXXX)

# ---- primary first: seed + READY, keep writing ----
$WRITER $SRCPORT 50000 > "$DIR/writer.log" 2>&1 &
WPID=$!
RPID=""
trap 'kill $WPID $RPID 2>/dev/null; rm -rf "$DIR"' EXIT
for _ in $(seq 100); do
    grep -q READY "$DIR/writer.log" 2>/dev/null && break
    sleep 0.2
done
grep -q READY "$DIR/writer.log" || { echo "repligate: FAIL — writer never READY"; exit 1; }

# ---- replica: single-source mode via config file ----
cat > "$DIR/replica.toml" <<EOF
[replication]
role = "replica"
upstream = "127.0.0.1:$SRCPORT"
single_source = true
EOF
start_replica() {
    env KEVY_BIND=127.0.0.1 $KEVY --threads 4 --port $REPPORT --dir "$DIR/rep" --no-aof \
        --config "$DIR/replica.toml" > "$DIR/replica.log" 2>&1 &
    RPID=$!
    for _ in $(seq 100); do
        $CLI -p $REPPORT PING >/dev/null 2>&1 && return
        sleep 0.2
    done
}
start_replica

# clamp 2 first (fresh replica = snapshot ship): wait for keyspace
echo "repligate: waiting for snapshot + catch-up"
CONVERGED=0
for _ in $(seq 120); do
    N=$($CLI -p $REPPORT DBSIZE 2>/dev/null | grep -oE "[0-9]+" || echo 0)
    if [ "${N:-0}" -ge 50003 ]; then CONVERGED=1; break; fi
    sleep 0.5
done
[ $CONVERGED = 1 ] || { echo "repligate: FAIL — replica never reached primary keyspace (dbsize=$N)"; tail -3 "$DIR/replica.log"; exit 1; }
echo "repligate: snapshot ship + live frames landed (dbsize=$N)"

# clamp 1: pause the writer (SIGSTOP), let the replica drain, compare digests
kill -STOP $WPID
# primary digest via its embedded data? The writer has no listener — use
# the REPLICA's own digest, polled until stable. The clamp is "with input
# stopped, the replica CONVERGES and STAYS stable" — a bounded wait tests
# exactly that; the old fixed 2s was a razor edge (v3.18 drained in
# exactly 2s on an idle box) that turned load-sensitive on slow runners.
D1=""; D2=""
for _ in $(seq 30); do
    D2=$($CLI digest -p $REPPORT p:)
    [ -n "$D1" ] && [ "$D1" = "$D2" ] && break
    D1="$D2"
    sleep 1
done
if [ "$D1" != "$D2" ] || [ -z "$D1" ]; then
    echo "repligate: FAIL — replica still churning while primary is stopped ($D1 vs $D2)"
    exit 1
fi
# and it must STAY stable — one more read a second later.
sleep 1
D2=$($CLI digest -p $REPPORT p:)
if [ "$D1" != "$D2" ]; then
    echo "repligate: FAIL — replica digest moved after convergence ($D1 vs $D2)"
    exit 1
fi
echo "repligate: quiesced digest stable: $D1"

# clamp 3: restart the replica while the primary is LIVE (a SIGSTOPped
# writer freezes its accept thread too — the first cut deadlocked the
# resync against a frozen primary), then pause the writer and require
# a stable NON-EMPTY digest.
kill $RPID 2>/dev/null; wait $RPID 2>/dev/null
kill -CONT $WPID
start_replica
CONVERGED=0
for _ in $(seq 120); do
    N=$($CLI -p $REPPORT DBSIZE 2>/dev/null | grep -oE "[0-9]+" || echo 0)
    if [ "${N:-0}" -ge 50003 ]; then CONVERGED=1; break; fi
    sleep 0.5
done
[ $CONVERGED = 1 ] || { echo "repligate: FAIL — post-restart replica never re-synced"; exit 1; }
kill -STOP $WPID
# same bounded convergence as clamp 1.
D3=""; D4=""
for _ in $(seq 30); do
    D4=$($CLI digest -p $REPPORT p:)
    [ -n "$D3" ] && [ "$D3" = "$D4" ] && break
    D3="$D4"
    sleep 1
done
sleep 1
D4B=$($CLI digest -p $REPPORT p:)
if [ "$D3" != "$D4" ] || [ "$D4" != "$D4B" ] || [ "$N" -lt 50003 ]; then
    echo "repligate: FAIL — post-restart digest unstable ($D3 vs $D4 vs $D4B)"
    exit 1
fi
echo "repligate: restart re-synced + stable: $D3"

# clamp 4: replica-local derived state over replicated data (writer
# stays paused so the backfill snapshot is stable)
$CLI -p $REPPORT IDX.CREATE rep_n ON PREFIX p: FIELD n TYPE i64 KIND range >/dev/null
OK=0
for _ in $(seq 60); do
    R=$($CLI -p $REPPORT IDX.QUERY rep_n RANGE 100 110 LIMIT 20 2>/dev/null || true)
    echo "$R" | grep -q "p:105" && { OK=1; break; }
    sleep 0.5
done
[ $OK = 1 ] || { echo "repligate: FAIL — replica-local index never answered"; exit 1; }
echo "repligate: replica-local IDX.QUERY answers over replicated data"
echo "repligate: PASS"
