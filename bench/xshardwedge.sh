#!/usr/bin/env bash
# xshardwedge — reproduction harness for the node-redis SET wedge.
#
# STATUS: RED, on purpose, and deliberately NOT wired into CI (the same
# gate-first discipline crashgate and uringgate shipped under — a
# permanently-red required gate teaches people to ignore gates).
#
# THE BUG. node-redis connects, completes its handshake, PINGs, FLUSHALLs,
# and then hangs forever on SET. The server does not think anything is
# wrong: CLIENT LIST shows the connection with `qbuf=0 oll=0 omem=0
# cmd=NULL`, i.e. all input consumed and no output pending, and every shard
# thread sits at 0.0% CPU in state S. Nothing is spinning; nothing is
# waiting on a socket. A message is stranded and no one will ever look for
# it again.
#
# This predates the two wedges fixed on 2026-07-19 (the blocked-timeout seq
# retire, 8d8f20e9, and the chunked-writev short-write prefix, 17c7062f) and
# is not either of them — it reproduces with both applied. It is the reason
# the `clientgate` CI job has been red.
#
# WHAT THE MATRIX SAYS (lx64, kernel 6.12.95, 6 node runs per cell):
#
#     io_uring 16 shards    6/6 wedged
#     io_uring  4 shards    1/6 wedged
#     io_uring  1 shard     0/6 wedged
#     epoll    16 shards    0/6 wedged
#
# It scales with cross-shard traffic and vanishes without it. FLUSHALL fans
# out to every shard, which is what makes the run cross-shard-heavy. Do NOT
# read the epoll row as immunity: the cross-core machinery is shared code,
# and epoll's inline flush only changes how often the path is taken.
#
# WHAT HAS BEEN RULED OUT
#   * Not a lost request at the parser: qbuf=0 with the client blocked.
#   * Not a busy-loop: every shard thread is asleep at 0% CPU.
#   * Not RESP framing desync: a strict replay of node-redis's exact
#     handshake bytes, parsing and matching all four replies, is 200/200
#     correct, and SET/GET/INCRBY after it are correct too.
#   * Not the backlog dirty-bit race (8368bb09). That race is real and is
#     fixed, but the wedge reproduces unchanged with the fix applied.
#
# NOT YET REPRODUCED WITHOUT node-redis. Raw-socket replays of its byte
# stream — including the exact 230-byte pipelined handshake, and a
# two-connection concurrent variant — do not wedge. Whatever node does
# differently is still unidentified, so this harness needs node + the
# `redis` package. That dependency is a known weakness of this repro, not a
# property of the bug.
#
# NEXT STEP, when someone picks this up: the useful artifact is per-shard
# state at the moment of the wedge — xshard_inflight, each backlog length,
# the inbound_dirty bitmap, arm_pending, and the pending-slot queue of the
# stuck conn. Everything is asleep, so a dump triggered by a signal or a
# debug command would name the stranded item directly instead of another
# round of hypotheses.
#
#   bash bench/xshardwedge.sh [KEVY_BIN]
#
# Exit codes: 0 = no wedge observed, 1 = wedge reproduced, 2 = refused.
set -uo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"
BIN=${1:-$HERE/target/release/kevy}
RUNS=${XSHARDWEDGE_RUNS:-6}
BASE_PORT=${XSHARDWEDGE_PORT:-7801}

refuse() { echo "xshardwedge: REFUSED — $1" >&2; exit 2; }
[ -x "$BIN" ] || refuse "no kevy binary at $BIN (cargo build --release -p kevy)"
command -v node >/dev/null || refuse "node not found (this repro needs node-redis)"

APP=$(mktemp -d)
SRV=""
cleanup() {
  [ -n "$SRV" ] && kill "$SRV" 2>/dev/null
  wait "$SRV" 2>/dev/null
  rm -rf "$APP"
}
trap cleanup EXIT

cp bench/clientgate/node_redis.mjs "$APP/" || refuse "missing bench/clientgate/node_redis.mjs"
(cd "$APP" && npm init -y >/dev/null 2>&1 && npm install --no-audit --no-fund --quiet redis >/dev/null 2>&1) \
  || refuse "npm install redis failed"

wedged=0
for i in $(seq "$RUNS"); do
  port=$((BASE_PORT + i))
  data=$(mktemp -d)
  env KEVY_BIND=127.0.0.1 "$BIN" --threads 16 --port "$port" --dir "$data" \
    > "$APP/srv-$i.log" 2>&1 &
  SRV=$!
  sleep 1.5
  if timeout 12 env KEVY_PORT="$port" node "$APP/node_redis.mjs" > "$APP/run-$i.log" 2>&1; then
    echo "xshardwedge: run $i ok"
  else
    wedged=$((wedged + 1))
    echo "xshardwedge: run $i WEDGED at [$(tail -1 "$APP/run-$i.log")]"
    # The connection state at the wedge is the whole point — capture it
    # while the server is still up.
    if [ -x target/release/kevy-cli ]; then
      target/release/kevy-cli -p "$port" CLIENT LIST 2>&1 | head -4
    fi
  fi
  kill "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; SRV=""
  rm -rf "$data"
done

echo "xshardwedge: $wedged/$RUNS wedged"
[ "$wedged" -eq 0 ] || exit 1
