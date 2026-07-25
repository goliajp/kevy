#!/bin/bash
# clientgate — the "bring your redis client" contract, made executable.
#
# kevy speaks RESP, so the claim is: the mainstream redis client of every
# ecosystem connects to a kevy server and just works — strings, TTL,
# hashes, lists, zsets, pub/sub, and the extended verb surface (IDX.* …)
# through each client's raw-command channel. This gate pins that claim
# against six clients:
#
#   node-redis / ioredis (Node) · go-redis (Go) · StackExchange.Redis
#   (.NET) · hiredis (C) · redis-py (Python)
#
# Every smoke runs the same ladder (see bench/clientgate/*): typed ops
# per family, one pub/sub round trip, one extended verb (IDX.LIST) raw.
# Any red = FAIL. Toolchains are required — a missing one fails loudly
# rather than skipping silently (CI installs all six).
#
# Usage: bash bench/clientgate.sh [path-to-kevy]
set -u
cd "$(dirname "$0")/.."

KEVY=${1:-target/release/kevy}
[ -x "$KEVY" ] || { echo "clientgate: build first: cargo build --release -p kevy"; exit 2; }

PORT=7501
DIR=$(mktemp -d /tmp/kevy-clientgate-XXXXXX)
SRVPID=""
cleanup() { kill $SRVPID 2>/dev/null; rm -rf "$DIR"; }
trap cleanup EXIT

env KEVY_BIND=127.0.0.1 "$KEVY" --port $PORT --dir "$DIR/data" > "$DIR/server.log" 2>&1 &
SRVPID=$!
for _ in $(seq 100); do
    target/release/kevy-cli -p $PORT PING >/dev/null 2>&1 && break
    sleep 0.2
done
target/release/kevy-cli -p $PORT PING >/dev/null 2>&1 || {
    echo "clientgate: FAIL — server never came up"; tail -5 "$DIR/server.log"; exit 1
}

# Non-interactive shells don't get nvm's lazy-loaded node/npm.
if ! command -v npm >/dev/null 2>&1; then
    nvm_bin=$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -1)
    [ -n "$nvm_bin" ] && export PATH="$nvm_bin:$PATH"
fi
command -v dotnet >/dev/null 2>&1 || export PATH="/opt/homebrew/opt/dotnet@8/bin:$PATH"
export DOTNET_CLI_TELEMETRY_OPTOUT=1

fail=0
run() { # run <name> <cmd...>
    local name=$1; shift
    echo "clientgate: ${name}..."
    # Bounded: a wedged runner (or a client that never times out its
    # connect) must fail loudly in minutes, not hang the job for hours
    # — a hosted-runner incident turned this gate into a 3× multi-hour
    # zombie with zero output. macOS dev boxes without coreutils
    # `timeout` run unbounded, same posture as repligate.
    local clamp=""
    command -v timeout >/dev/null 2>&1 && clamp="timeout 180"
    if $clamp "$@" > "$DIR/$name.log" 2>&1; then
        echo "clientgate: $name OK"
    else
        echo "clientgate: $name FAIL (or timed out at 180s)"
        tail -10 "$DIR/$name.log"; fail=1
        # Server-side view of the failure: who is connected, and what
        # the server said — the client log alone was blank once.
        target/release/kevy-cli -p $PORT CLIENT LIST 2>&1 | head -5
        tail -5 "$DIR/server.log"
    fi
}

# ── Node: node-redis + ioredis, one npm install for both ──
NODEAPP="$DIR/nodeapp"
mkdir -p "$NODEAPP"
cp bench/clientgate/node_redis.mjs bench/clientgate/ioredis.mjs "$NODEAPP/"
(cd "$NODEAPP" && npm init -y >/dev/null 2>&1 \
    && npm install --no-audit --no-fund --quiet redis ioredis >/dev/null 2>&1)
run node-redis env KEVY_PORT=$PORT node "$NODEAPP/node_redis.mjs"
run ioredis    env KEVY_PORT=$PORT node "$NODEAPP/ioredis.mjs"

# ── Go: go-redis (go run resolves modules from its cwd) ──
run go-redis bash -c "cd bench/clientgate/goredis && KEVY_PORT=$PORT go run ."

# ── .NET: StackExchange.Redis ──
run se-redis env KEVY_PORT=$PORT dotnet run --project bench/clientgate/seredis -c Release

# ── Python: redis-py, sync + asyncio (venv, no global pollution) ──
# The async client (redis.asyncio) ships in the same `redis` package, so
# both smokes run off one install — same ladder, sync vs await.
python3 -m venv "$DIR/venv" >/dev/null 2>&1 \
    && "$DIR/venv/bin/pip" install --quiet redis >/dev/null 2>&1
run redis-py       env KEVY_PORT=$PORT "$DIR/venv/bin/python" bench/clientgate/redispy.py
run redis-py-async env KEVY_PORT=$PORT "$DIR/venv/bin/python" bench/clientgate/redispy_async.py

# ── C: hiredis ──
HIREDIS_PREFIX=""
for p in /opt/homebrew/opt/hiredis /usr/local /usr; do
    [ -f "$p/include/hiredis/hiredis.h" ] && HIREDIS_PREFIX=$p && break
done
if [ -z "$HIREDIS_PREFIX" ]; then
    echo "clientgate: hiredis FAIL — hiredis not installed (brew install hiredis / apt install libhiredis-dev)"
    fail=1
else
    cc -I"$HIREDIS_PREFIX/include" -L"$HIREDIS_PREFIX/lib" \
        bench/clientgate/hiredis_smoke.c -lhiredis -o "$DIR/hiredis_smoke"
    run hiredis env KEVY_PORT=$PORT "$DIR/hiredis_smoke"
fi

if [ $fail = 0 ]; then
    echo "clientgate: PASS — six clients, one server, zero adapters"
else
    echo "clientgate: FAIL"
    exit 1
fi
