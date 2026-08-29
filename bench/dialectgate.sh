#!/usr/bin/env bash
# dialectgate — the Lua dialect surface, pinned per dialect (v5.3 suite).
#
# 5.2 corrected the 5.1/5.2 dialects against upstream Lua, and that
# correction is user-visible through EVAL: the table library follows the
# dialect, error wording is operand-first on <=5.2 and type-first on
# >=5.3, and a denial of service (string.rep with an empty piece) is
# fixed. Every one of those is exactly the kind of thing a dependency
# bump quietly un-fixes, so this gate pins them against a real server —
# the unit tests inside kevy-lua pin the bridge, this pins what a client
# on the wire observes.
#
# Every probe is an assertion; the count is the floor (a gate that ran
# nothing must not pass). The DoS probe is time-bounded by the gate's
# own deadline: if the VM hangs, the read times out and the gate is red.
#
# usage: dialectgate.sh <kevy-binary>
set -u

BIN="${1:?usage: dialectgate.sh <kevy-binary>}"
DIR="$(mktemp -d "${TMPDIR:-/tmp}/kevy-dialectgate-XXXXXX")"
# A random port with no bind check lost its first race on the machine
# that wrote it. Ask the kernel instead — bind :0, read, release; the
# window between here and the server binding is the wait loop's problem,
# and the wait loop fails loudly.
PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")
trap 'kill $SRV 2>/dev/null; wait $SRV 2>/dev/null; rm -rf "$DIR"' EXIT

"$BIN" --port "$PORT" --dir "$DIR" --no-aof >/dev/null 2>&1 &
SRV=$!

python3 - "$PORT" <<'PY'
import os, socket, sys, time

port = int(sys.argv[1])

# 60 s, not 15. A debug build on a machine already running the rest of the
# suite took 15.2 s to accept and this gate reported it as "never" — the
# same lesson `crates/kevy-cli/tests/migrate_roundtrip.rs` wrote down when it
# went from 2 s to 10: a wait budget tuned on an idle machine is a flake
# scheduled for the first busy one. The server was fine; hand-started it
# listens, answers PING, and prints its banner.
#
# Sixty is still not always enough. This gate failed on a workstation that
# was compiling an unrelated project — thirty rustc processes, none of them
# this one's — and the answer to that is not a larger constant, it is the
# knob the rest of the suite already turns: KEVY_TEST_PATIENCE, which
# covgate and deadgate set to 6 and the replication tests read for the same
# reason. A budget tuned on an idle machine is a flake scheduled for the
# first busy one, and the second busy one after that.
budget = 60 * float(os.environ.get("KEVY_TEST_PATIENCE", "1"))
deadline = time.time() + budget
while time.time() < deadline:
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=1)
        break
    except OSError:
        time.sleep(0.2)
else:
    sys.exit(f"dialectgate: server never accepted on 127.0.0.1:{port} "
             f"within {budget:.0f}s — it is slow or absent, and this cannot "
             f"tell which; check the server log in the work dir. On a loaded "
             f"machine widen it with KEVY_TEST_PATIENCE=3.")
s.settimeout(10)

def ev(script):
    a = ["EVAL", script, "0"]
    s.sendall(f"*{len(a)}\r\n".encode() + b"".join(f"${len(x)}\r\n{x}\r\n".encode() for x in a))
    return s.recv(1 << 16).decode(errors="replace")

checks, failed = 0, []

def pin(name, reply, want):
    global checks
    checks += 1
    if want not in reply:
        failed.append(f"{name}: wanted {want!r} in {reply[:120]!r}")

# ── the 5.1 default: real Lua 5.1's table library ────────────────────
pin("5.1 table.unpack absent", ev("return type(table.unpack)"), "nil")
pin("5.1 table.create absent", ev("return type(table.create)"), "nil")
pin("5.1 table.setn present", ev("return type(table.setn)"), "function")
pin("5.1 setn raises upstream's error", ev("return table.setn({},1)"), "'setn' is obsolete")
pin("5.1 global unpack works", ev("local t={1,2,3}; return {unpack(t)}"), "*3")

# ── error wording: operand-first on <=5.2, type-first on >=5.3 ───────
pin("5.1 call error operand-first",
    ev("local f = nil; return f()"), "attempt to call local 'f' (a nil value)")
pin("5.1 index error operand-first",
    ev("local t = nil; return t.x"), "attempt to index local 't' (a nil value)")
pin("5.2 call error operand-first",
    ev("#!lua version=5.2\nlocal f = nil; return f()"),
    "attempt to call local 'f' (a nil value)")
pin("5.3 call error type-first",
    ev("#!lua version=5.3\nlocal f = nil; return f()"),
    "attempt to call a nil value (local 'f')")

# ── dialect opt-in gets the dialect's own surface ────────────────────
pin("5.4 table.unpack present",
    ev("#!lua version=5.4\nreturn {table.unpack({1,2,3})}"), "*3")
pin("5.4 table.setn absent",
    ev("#!lua version=5.4\nreturn type(table.setn)"), "nil")
pin("5.5 answers", ev("#!lua version=5.5\nreturn 1"), ":1")
pin("unknown dialect refused", ev("#!lua version=9.9\nreturn 1"), "unknown lua version")

# ── the DoS stays fixed: bounded by this connection's own timeout ────
t0 = time.time()
r = ev('return string.rep("", math.maxinteger, "")')
took = time.time() - t0
checks += 1
if took > 5:
    failed.append(f"string.rep DoS: call took {took:.1f}s — the hang is back")

# ── RESP3 negotiation serves the same dialect ────────────────────────
s.sendall(b"*2\r\n$5\r\nHELLO\r\n$1\r\n3\r\n")
s.recv(1 << 16)
pin("RESP3: same 5.1 surface", ev("return type(table.unpack)"), "nil")

s.close()

if checks < 15:
    sys.exit(f"dialectgate: only {checks} probes ran — the gate is broken, not green")
if failed:
    print(f"dialectgate: FAIL — {len(failed)} of {checks} pins broken")
    for f in failed:
        print(f"  ✗ {f}")
    sys.exit(1)
print(f"dialectgate: PASS ({checks} pins across dialects 5.1–5.5, both protocols)")
PY
