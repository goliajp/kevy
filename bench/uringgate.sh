#!/usr/bin/env bash
# uringgate — the io_uring data-plane contract, executed.
#
# WHY THIS GATE EXISTS (2026-07-18). Two connection-wedging bugs shipped
# and survived every fast test layer, because that layer never runs the
# reactor they live in:
#
#   * every Rust integration test forces `KEVY_IO_URING=0` (see
#     crates/kevy/tests/replication.rs, kevy-cluster-rw/tests/rw_split.rs,
#     kevy-embedded/tests/server_replica_e2e.rs) — the per-PR test matrix
#     is an EPOLL matrix;
#   * the gates that do exercise uring (clientgate / conformance /
#     availgate) run it by default, and they DID catch both bugs — but
#     only probabilistically, as a 1-in-N flake under load, which reads
#     like CI noise and got retried away for nights.
#
# So this gate is the deterministic middle: uring forced ON, driving
# exactly the two paths those bugs lived in, tight enough that a
# regression shows up as a FAIL in seconds instead of a flake in days.
#
#   sq-pressure    pub/sub fan-out + many conns arming at once fills the
#                  SQ; a recv re-arm the ring refused must be retried, not
#                  dropped (76c79c38 — dropped it, the conn wedged with
#                  no armed recv and nothing to re-trigger it)
#   blocking-tail  a blocking command that times out, then MORE commands
#                  on the same conn: the multishot recv can terminate with
#                  res=0 + IORING_CQE_F_SOCK_NONEMPTY ("more data, re-arm
#                  me"), which is NOT EOF (667005f9 — read as EOF, closed,
#                  and stranded the next request in a provided buffer)
#
# A wedge is observable without reading kernel state: the client stops
# getting replies. Every request here carries a deadline; a missed reply
# is a wedge is a FAIL.
#
#   bash bench/uringgate.sh [KEVY_BIN]
#
# STATUS 2026-07-19: GREEN, and wired into CI. It was committed RED on
# purpose the day before (crashgate's gate-first discipline) and stayed red
# for both shipped uring fixes, failing at "the command after the blocking
# timeout" within ~3 rounds. Chasing the recv side further was the wrong
# lead: triage during a live wedge showed a FRESH connection to the same
# server answering PING normally, which cleared the reactor loop and put
# the fault in per-conn state. The bug was neither in io_uring nor in recv
# — `tick_blocked_timeouts` resolved a parked command without retiring its
# seq, so the conn ran one behind forever and the first reply to take the
# pending path landed in a slot that was never allocated (8d8f20e9).
#
# That bug is reactor-agnostic, which is why the blocking-tail scenario now
# runs on epoll and macOS too; only sq-pressure needs a real ring. A box
# with no io_uring covers half this gate instead of none of it.
#
# Exit codes: 0 = PASS, 1 = FAIL (a conn wedged), 2 = refused (missing
# tool / server never came up).
set -uo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BIN=${1:-$HERE/target/release/kevy}
PORT=${URINGGATE_PORT:-7431}
SHARDS=${URINGGATE_SHARDS:-4}
ROUNDS=${URINGGATE_ROUNDS:-60}
DEADLINE=${URINGGATE_DEADLINE:-5}

refuse() { echo "uringgate: REFUSED — $1" >&2; exit 2; }
command -v python3 >/dev/null || refuse "python3 not found"
[ -x "$BIN" ] || {
  cargo build -q --release -p kevy --bin kevy || refuse "cannot build kevy"
  BIN="$HERE/target/release/kevy"
}

DIR=$(mktemp -d)
SRV=""
cleanup() {
  [ -n "$SRV" ] && kill "$SRV" 2>/dev/null
  wait "$SRV" 2>/dev/null
  rm -rf "$DIR"
}
trap cleanup EXIT

# Deliberately NOT forcing KEVY_IO_URING: the auto path is what production
# and every other gate runs, and it is the ONLY path that reports which
# reactor it picked (`reactor_choice` prints just for the unset case —
# forcing the env var silently skips the line, which is how the first cut
# of this gate managed to SKIP on a perfectly uring-capable box). Auto
# also means this gate asserts the real default, not a special mode.
env KEVY_BIND=127.0.0.1 "$BIN" \
  --port "$PORT" --threads "$SHARDS" --dir "$DIR/data" > "$DIR/srv.log" 2>&1 &
SRV=$!
for _ in $(seq 100); do
  python3 - "$PORT" <<'PY' >/dev/null 2>&1 && break
import socket, sys
socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.2).close()
PY
  sleep 0.1
done
# A server that never came up must say so. Without this the first client
# connect fails with ECONNREFUSED and the gate reports it as a round-0
# wedge — which is what a back-to-back run did when the previous server
# still held the port and this one silently failed to bind.
kill -0 "$SRV" 2>/dev/null || refuse "server exited during startup (see below)
$(tail -20 "$DIR/srv.log")"
python3 - "$PORT" <<'PY' >/dev/null 2>&1 || refuse "server not accepting on port $PORT
$(tail -20 "$DIR/srv.log")"
import socket, sys
socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1.0).close()
PY

REACTOR=$(grep -m1 -o "reactor = [a-z_]*" "$DIR/srv.log" 2>/dev/null || true)
# The two scenarios have different reach. sq-pressure targets the io_uring
# submission queue specifically and is meaningless elsewhere. blocking-tail is
# reactor-agnostic: it caught a seq-retire bug in the shared blocked-client
# registry that wedged epoll exactly as it wedged io_uring, so skipping it off
# the ring would have left a CI box covering nothing while looking green.
case "$REACTOR" in
  *io_uring*) URING=1; echo "uringgate: $REACTOR, ${SHARDS} shards, ${ROUNDS} rounds" ;;
  *epoll*)
    URING=0
    echo "uringgate: epoll (no usable io_uring) — blocking-tail only, ${ROUNDS} rounds"
    ;;
  *)
    # No reactor line at all = non-Linux (macOS/kqueue), where
    # `reactor_choice` doesn't print one.
    URING=0
    echo "uringgate: not a Linux io_uring box — blocking-tail only, ${ROUNDS} rounds"
    ;;
esac

python3 - "$PORT" "$ROUNDS" "$DEADLINE" "$URING" <<'PY'
import socket, sys, time

port, rounds, deadline = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
uring = sys.argv[4] == "1"

def enc(*parts):
    out = b"*%d\r\n" % len(parts)
    for p in parts:
        if isinstance(p, str):
            p = p.encode()
        out += b"$%d\r\n%s\r\n" % (len(p), p)
    return out

class Conn:
    """One RESP connection with a deadline on every reply. A wedged
    connection shows up here as a socket timeout, which is the whole
    point of the gate."""
    def __init__(self, tag):
        self.tag = tag
        self.s = socket.create_connection(("127.0.0.1", port), 5)
        self.s.settimeout(deadline)
        self.buf = b""

    def send(self, *parts):
        self.s.sendall(enc(*parts))

    def line(self):
        while b"\r\n" not in self.buf:
            chunk = self.s.recv(65536)
            if not chunk:
                raise AssertionError(f"{self.tag}: peer closed mid-reply")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def reply(self):
        """One complete RESP reply (enough shape for the verbs used here)."""
        head = self.line()
        tag = head[:1]
        if tag in (b"+", b"-", b":"):
            return head
        if tag == b"$":
            n = int(head[1:])
            if n < 0:
                return None
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(65536)
            body, self.buf = self.buf[:n], self.buf[n + 2:]
            return body
        if tag == b"*":
            n = int(head[1:])
            if n < 0:
                return None
            return [self.reply() for _ in range(n)]
        raise AssertionError(f"{self.tag}: unknown RESP tag {head!r}")

    def cmd(self, *parts):
        self.send(*parts)
        return self.reply()

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass

STEP = ["(none)"]

def step(name):
    """Where we are, so a deadline miss names the exact operation that
    stopped getting replies instead of just 'somewhere in this round'."""
    STEP[0] = name

def check(cond, what):
    if not cond:
        raise AssertionError(what)

# ── big-arg-pipeline: a deep pipeline of values past the promote line ────
# Values >= BIG_ARG_PROMOTE_THRESHOLD (4 KiB) route through the big-arg
# machinery. A 512-deep pipeline of them keeps the buffer ring under
# pressure, so the multishot recv terminates on its own (ENOBUFS) mid
# frame — routinely, not rarely. The `Frame` variant (cross-shard
# bare-SET, i.e. most keys on a multi-shard instance) needs that
# multishot re-armed to finish stitching; when the arm pass refused to
# re-arm any conn with a big-arg in flight, the frame never completed and
# the conn wedged with no armed recv and nothing queued to fix it
# (`big_arg=Frame(3232/4132) recv_armed=false arm_queued=false`, ~1 wedge
# per 2 runs of 120k keys). Deep pipelines of large values are ordinary
# bulk-load client behaviour, so this is a first-class contract.
def round_big_arg_pipeline(i):
    step("bap: connect")
    c = Conn(f"bap{i}")
    try:
        for size in (4096, 8192):
            val = bytes((i + size) % 251 for _ in range(size))
            step(f"bap: pipeline 512 x SET {size}B")
            frames = b"".join(
                enc("SET", f"bap:{i}:{size}:{k}", val) for k in range(512)
            )
            c.s.sendall(frames)
            for k in range(512):
                step(f"bap: reply {k + 1}/512 ({size}B values)")
                check(c.reply() == b"+OK", f"SET reply {k} at {size}B")
        step("bap: read back after the pipeline")
        check(len(c.cmd("GET", f"bap:{i}:4096:0")) == 4096, "value round-trips")
        step("bap: conn still serves after the pipeline")
        check(c.cmd("PING") == b"+PONG", "PING after big-arg pipeline")
    finally:
        c.close()

# ── sq-pressure: pub/sub fan-out while many conns arm at once ────────────
# A publish to N subscribers queues N writes in the same iteration; every
# subscriber conn also wants a recv arm. That is the ring-full window where
# a dropped re-arm used to strand a connection forever.
def round_sq_pressure(i):
    step("sq: connect 8 subscribers")
    subs = [Conn(f"sub{k}") for k in range(8)]
    try:
        for k, c in enumerate(subs):
            step(f"sq: SUBSCRIBE #{k}")
            check(c.cmd("SUBSCRIBE", "room") is not None, "subscribe ack")
        step("sq: connect publisher")
        pub = Conn("pub")
        try:
            step("sq: PUBLISH fan-out")
            n = pub.cmd("PUBLISH", "room", "x" * 512)
            # Subscribers from the previous round may not be reaped yet, so
            # the count is a floor, not an equality — this gate is about
            # wedges, not about reap timing.
            check(n.startswith(b":") and int(n[1:]) >= 8, f"publish reached {n!r}, want >= 8")
            # Every subscriber must actually receive it — a wedged conn
            # times out here.
            for k, c in enumerate(subs):
                step(f"sq: fan-out delivery #{k}")
                msg = c.reply()
                check(msg is not None and msg[0] == b"message", "fan-out delivery")
            # And the publisher must still be able to serve a normal
            # command after the fan-out (its own recv must still be armed).
            step("sq: publisher PING after fan-out")
            check(pub.cmd("PING") == b"+PONG", "publisher alive after fan-out")
        finally:
            pub.close()
    finally:
        for c in subs:
            c.close()

# ── blocking-tail: a blocking timeout, then MORE commands ───────────────
# The exact 667005f9 shape: BLPOP on an empty key times out, and the very
# next request on the same connection is the one the multishot recv used
# to strand (res=0 + F_SOCK_NONEMPTY read as EOF).
def round_blocking_tail(i):
    # Per-round keys: BLPOP pops only one element, so a shared key would
    # carry leftovers into the next round and make the arity checks lie
    # about what the server did.
    bl, bz = f"bl{i}", f"bz{i}"
    c = Conn("block")
    try:
        step("bt: RPUSH")
        check(c.cmd("RPUSH", bl, "a", "b") == b":2", "rpush")
        step("bt: BLPOP immediate hit")
        hit = c.cmd("BLPOP", bl, "1")
        check(hit is not None, "blpop immediate hit")
        # The blocking timeout — this is what arms the race.
        step("bt: BLPOP timeout (arms the race)")
        empty = c.cmd("BLPOP", f"nosuchkey{i}", "0.1")
        check(empty is None, "blpop timeout returns nil")
        # …and now the requests that used to be stranded.
        step("bt: ZADD after the blocking timeout")
        check(c.cmd("ZADD", bz, "5", "lo", "9", "hi") == b":2", "zadd after timeout")
        step("bt: BZPOPMIN after the blocking timeout")
        z = c.cmd("BZPOPMIN", bz, "1")
        check(z is not None, "bzpopmin immediate hit after a blocking timeout")
        step("bt: SET after BZPOPMIN")
        check(c.cmd("SET", f"k{i}", "v") == b"+OK", "set after bzpopmin")
        step("bt: GET after BZPOPMIN")
        check(c.cmd("GET", f"k{i}") == b"v", "get after bzpopmin")
    finally:
        c.close()

t0 = time.time()
try:
    for i in range(rounds):
        if uring:
            round_sq_pressure(i)
            round_big_arg_pipeline(i)
        round_blocking_tail(i)
except (AssertionError, socket.timeout, OSError) as e:
    kind = "WEDGED (no reply within the deadline)" if isinstance(e, socket.timeout) else str(e)
    print(f"uringgate: FAIL — round {i}, step [{STEP[0]}]: {kind}")
    # Triage while the wedge is still live: is the SHARD stuck, or only
    # this connection? A fresh conn that answers proves the reactor is
    # running and the fault is per-conn state (recv arm / write queue);
    # a fresh conn that also hangs proves the loop itself is stuck.
    if isinstance(e, socket.timeout):
        try:
            probe = Conn("probe")
            probe.s.settimeout(2.0)
            pong = probe.cmd("PING")
            print(f"uringgate: triage — fresh conn to the same server: {pong!r}"
                  f" => {'SHARD ALIVE, per-conn wedge' if pong else 'shard stuck too'}")
            probe.close()
        except Exception as pe:
            print(f"uringgate: triage — fresh conn ALSO hung ({pe!r})"
                  f" => the reactor loop is stuck, not just the conn")
    sys.exit(1)

scen = "sq-pressure + big-arg-pipeline + blocking-tail" if uring else "blocking-tail"
print(f"uringgate: {rounds} rounds x ({scen}) in {time.time()-t0:.1f}s")
PY
rc=$?
[ $rc -eq 0 ] || { echo "uringgate: FAIL"; exit 1; }
echo "uringgate: PASS — no connection wedged under SQ pressure, a deep big-arg pipeline, or after a blocking timeout"
