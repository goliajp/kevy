#!/usr/bin/env bash
# resp3gate — the RESP3 shapes kevy claims to speak.
#
# The site says "RESP2 and RESP3 … your client library will not notice", in
# three languages. The override table in crates/kevy/src/dispatch_resp3.rs
# holds NINE verbs, compat3 contains zero occurrences of `-3`, and all six
# clientgate clients connect on the default protocol. So the claim had no
# judge at all, and the bit at stake — a reply's TYPE — is exactly what a
# type-decoding client reads (redis-py protocol=3, node-redis v5+).
#
# What Redis actually changes under RESP3 is not a matter of opinion, so this
# asks the pinned image rather than a table someone typed: for each verb, take
# the reply under RESP2 and under RESP3 from redis, and from kevy, and require
# that kevy differs between protocols exactly where redis does. A verb redis
# answers identically in both is not a RESP3 verb and is not required to move.
#
# Usage: bash bench/resp3gate.sh <kevy-binary>
set -uo pipefail
cd "$(dirname "$0")"
. ./anchor-lib.sh || { echo "resp3gate: cannot load anchor-lib.sh" >&2; exit 2; }
command -v anchor_pin >/dev/null || { echo "resp3gate: anchor_pin missing" >&2; exit 2; }

KBIN=${1:?usage: resp3gate.sh <kevy-binary>}
KBIN=$(cd "$(dirname "$KBIN")" && pwd)/$(basename "$KBIN")
RPORT=7431
KPORT=7432
RNAME=resp3gate-redis
PIN=$(anchor_pin redis)

cleanup() {
    docker rm -f "$RNAME" >/dev/null 2>&1
    [ -n "${KPID:-}" ] && kill "$KPID" 2>/dev/null
}
trap cleanup EXIT

docker rm -f "$RNAME" >/dev/null 2>&1
docker run -d --name "$RNAME" -p "127.0.0.1:$RPORT:6379" "redis:$PIN" >/dev/null \
    || { echo "resp3gate: could not start redis:$PIN" >&2; exit 2; }
for _ in $(seq 60); do
    docker exec "$RNAME" redis-cli ping 2>/dev/null | grep -q PONG && break
    sleep 0.5
done
served=$(docker exec "$RNAME" redis-cli INFO server 2>/dev/null | tr -d '\r' \
    | sed -n 's/^redis_version:\(.*\)$/\1/p' | head -1)
anchor_require "redis (resp3gate)" "$PIN" "$served"

env KEVY_BIND=127.0.0.1 "$KBIN" --port $KPORT --no-aof >/tmp/resp3gate-kevy.log 2>&1 &
KPID=$!
for _ in $(seq 60); do
    python3 -c "import socket,sys; socket.create_connection(('127.0.0.1',$KPORT),1).close()" 2>/dev/null && break
    sleep 0.5
done

python3 resp3gate.py "$RPORT" "$KPORT"
