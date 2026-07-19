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
# Skips cleanly (exit 0) where io_uring is unavailable — macOS dev boxes,
# containers with a seccomp profile that blocks io_uring_setup. On Linux
# CI runners and lx64 it runs for real.
#
#   bash bench/uringgate.sh [KEVY_BIN]
#
# STATUS 2026-07-18: RED, on purpose (same gate-first discipline crashgate
# shipped under). This gate reproduces a wedge that survives BOTH shipped
# uring fixes — typically within ~3 rounds on lx64 (6.12.95, 4 shards):
#
#   FAIL — round 3, step [bt: ZADD after the blocking timeout]: WEDGED
#
# Server-side tracing shows the command IS received and dispatched, and the
# reply never reaches the client; the terminating `res == 0` completion is
# indistinguishable between a closed peer and a live one, and every
# disambiguation tried so far (flag-only, bounded retry, a MSG_PEEK EOF
# probe) fixes one half and breaks the other. It is NOT wired into CI until
# it goes green — a permanently-red required gate teaches people to ignore
# gates. Run it by hand on a uring box; see
# bench/PERF-FINDING-2026-07-18-uring-recv-rearm-wedge.md for the trace
# evidence and the refuted hypotheses.
#
# What IS fixed and proven (real-kernel A/B, 25 conformance runs each):
# the two shipped fixes took the client-conformance blpop/bzpopmin hang
# from 4/25 to 0/25. This gate is simply a harder workload than that.
#
# Exit codes: 0 = PASS (or SKIP, no io_uring), 1 = FAIL (a conn wedged),
# 2 = refused (missing tool / server never came up).
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

REACTOR=$(grep -m1 -o "reactor = [a-z_]*" "$DIR/srv.log" 2>/dev/null || true)
case "$REACTOR" in
  *io_uring*) echo "uringgate: $REACTOR, ${SHARDS} shards, ${ROUNDS} rounds" ;;
  *epoll*)
    # Linux without a usable ring (pre-5.19, or seccomp-blocked as in a
    # default-profile container). Nothing to gate — but say which, so a CI
    # box that silently stopped covering uring is visible in the log.
    echo "uringgate: SKIP — this box fell back to epoll (no usable io_uring)"
    exit 0
    ;;
  *)
    # No reactor line at all = non-Linux (macOS/kqueue), where
    # `reactor_choice` doesn't print one.
    echo "uringgate: SKIP — not a Linux io_uring box"
    exit 0
    ;;
esac

python3 - "$PORT" "$ROUNDS" "$DEADLINE" <<'PY'
import socket, sys, time

port, rounds, deadline = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])

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
        round_sq_pressure(i)
        round_blocking_tail(i)
except (AssertionError, socket.timeout, OSError) as e:
    kind = "WEDGED (no reply within the deadline)" if isinstance(e, socket.timeout) else str(e)
    print(f"uringgate: FAIL — round {i}, step [{STEP[0]}]: {kind}")
    sys.exit(1)

print(f"uringgate: {rounds} rounds x (sq-pressure + blocking-tail) in {time.time()-t0:.1f}s")
PY
rc=$?
[ $rc -eq 0 ] || { echo "uringgate: FAIL"; exit 1; }
echo "uringgate: PASS — no connection wedged under SQ pressure or after a blocking timeout"
