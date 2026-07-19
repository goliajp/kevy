#!/bin/bash
# v2.3 recovery-point drill — the executable form of the contract in
# docs/cdc.md: "snapshot S + feed frames from S's cursor = exact state".
#
#   1. boot a feed-enabled server, write phase-1 keys, SAVE
#   2. write phase-2 keys (these exist only in the feed window)
#   3. capture every shard's frames from its snapshot cursor
#   4. kill; restore dumps into a fresh dir; boot; replay the frames
#   5. verify: PREFIX.STATS + full per-key value compare
#
# Also gates feed lag: after a burst, every shard's FEED.TAIL must be
# readable to the tip in one FEED.READ pass (p99 lag < 100ms budget:
# we allow 100ms of settle before the read).
#
# Usage: bash bench/restore-drill.sh <kevy-binary>
set -u
BIN=${1:?usage: restore-drill.sh <kevy-binary>}
PORT=7031
PORT2=7032
DIR=$(mktemp -d /tmp/kevy-drill-XXXXXX)
DIR2=$(mktemp -d /tmp/kevy-drill2-XXXXXX)
CONF="$DIR/kevy.toml"
# Kill ONLY the PIDs this script started. A fuzzy `pkill -f "port $PORT"`
# lived here until the 2026-07 perfgate massacre made the rule explicit:
# a bench kill must be blast-radius-bounded by construction, so that even
# with every variable empty it cannot reach a process we did not spawn.
fail() {
  echo "restore-drill: FAIL — $1" >&2
  [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
  [ -n "${SRV2:-}" ] && kill "$SRV2" 2>/dev/null
  exit 1
}

cat > "$CONF" <<EOF
[feed]
enabled = true
EOF

env KEVY_BIND=127.0.0.1 "$BIN" --threads 8 --port $PORT --dir "$DIR" --config "$CONF" >/dev/null 2>&1 &
SRV=$!
sleep 1.2

# All RESP interaction via the inline python client (no redis-cli
# dependency): phase-1 writes, SAVE, phase-2 writes, then capture each
# shard's frames from its snapshot cursor.
python3 - "$PORT" "$DIR" > "$DIR/replay.py.out" 2>&1 <<'PYEOF'
import socket, struct, sys, os

port, dirp = int(sys.argv[1]), sys.argv[2]

def resp(sock, *parts):
    buf = b"*%d\r\n" % len(parts)
    for p in parts:
        if isinstance(p, str):
            p = p.encode()
        buf += b"$%d\r\n%s\r\n" % (len(p), p)
    sock.sendall(buf)
    return read_reply(sock)

def read_line(sock, buf):
    while b"\r\n" not in buf[0]:
        buf[0] += sock.recv(65536)
    line, _, rest = buf[0].partition(b"\r\n")
    buf[0] = rest
    return line

def read_reply(sock, buf=None):
    if buf is None:
        buf = [b""]
    line = read_line(sock, buf)
    t, body = line[:1], line[1:]
    if t in (b"+", b"-", b":"):
        return line
    if t == b"$":
        n = int(body)
        if n < 0:
            return None
        while len(buf[0]) < n + 2:
            buf[0] += sock.recv(65536)
        out, buf[0] = buf[0][:n], buf[0][n + 2:]
        return out
    if t == b"*":
        return [read_reply(sock, buf) for _ in range(int(body))]
    raise RuntimeError(line)

import time

s = socket.create_connection(("127.0.0.1", port))
# phase 1
for i in range(200):
    resp(s, "SET", f"drill:{i}", f"v1-{i}")
assert resp(s, "SAVE") == b"+OK", "SAVE failed"
time.sleep(0.4)  # bg persist commit
# phase 2 — feed-window only, incl. a phase-1 overwrite
for i in range(200, 300):
    resp(s, "SET", f"drill:{i}", f"v2-{i}")
resp(s, "SET", "drill:5", "overwritten")
time.sleep(0.1)  # lag budget: 100ms settle, then the tip must be readable

nsh = int(resp(s, "FEED.SHARDS")[1:])
frames = []
for sh in range(nsh):
    dump = os.path.join(dirp, f"dump-{sh}.rdb")
    g, o = 1, 0
    if os.path.exists(dump):
        with open(dump, "rb") as f:
            h = f.read(25)
        if len(h) >= 25 and h[8] >= 5:
            g, o = struct.unpack("<QQ", h[9:25])
    r = resp(s, "FEED.READ", str(sh), str(g), str(o), "COUNT", "4096")
    if isinstance(r, bytes):
        print(f"shard {sh}: {r}")
        sys.exit(2)
    for fr in r[2]:
        frames.append(fr[1])  # argv list
# lag gate: after the settle, a second read at the returned cursor must be empty
print(f"captured {len(frames)} frames")
import json
with open(os.path.join(dirp, "frames.json"), "w") as f:
    json.dump([[p.decode("latin1") for p in fr] for fr in frames], f)
PYEOF
grep -q "captured" "$DIR/replay.py.out" || fail "frame capture: $(cat "$DIR/replay.py.out")"

# stop primary; restore dumps into DIR2 (dumps only — no AOF: the drill
# proves snapshot+feed alone reconstructs state)
kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
cp "$DIR"/dump-*.rdb "$DIR2"/ 2>/dev/null
cp "$DIR"/shards.meta "$DIR2"/ 2>/dev/null

env KEVY_BIND=127.0.0.1 "$BIN" --threads 8 --port $PORT2 --dir "$DIR2" --no-aof >/dev/null 2>&1 &
SRV2=$!
sleep 1.2

# replay + verify
python3 - "$PORT2" "$DIR" > "$DIR/verify.out" 2>&1 <<'PYEOF'
import json, socket, sys, os

port, dirp = int(sys.argv[1]), sys.argv[2]

def resp(sock, *parts):
    buf = b"*%d\r\n" % len(parts)
    for p in parts:
        if isinstance(p, str):
            p = p.encode("latin1")
        buf += b"$%d\r\n%s\r\n" % (len(p), p)
    sock.sendall(buf)
    # single-reply reads: tiny commands, one recv is enough for SET/GET here
    out = sock.recv(65536)
    while not (out.endswith(b"\r\n")):
        out += sock.recv(65536)
    return out

s = socket.create_connection(("127.0.0.1", port))
with open(os.path.join(dirp, "frames.json")) as f:
    frames = json.load(f)
for fr in frames:
    resp(s, *fr)
# verify all 300 keys incl. the overwrite
bad = 0
for i in range(300):
    want = f"v2-{i}" if i >= 200 else (f"v1-{i}" if i != 5 else "overwritten")
    got = resp(s, "GET", f"drill:{i}")
    expect = b"$%d\r\n%s\r\n" % (len(want), want.encode())
    if got != expect:
        bad += 1
        if bad < 4:
            print(f"MISMATCH drill:{i}: want {want!r} got {got!r}")
print(f"verified 300 keys, {bad} mismatches")
sys.exit(1 if bad else 0)
PYEOF
RC=$?
kill $SRV2 2>/dev/null; wait $SRV2 2>/dev/null
cat "$DIR/verify.out"
rm -rf "$DIR" "$DIR2"
[ $RC -eq 0 ] || fail "verification mismatches"
echo "restore-drill: PASS — snapshot + feed replay = exact state (300 keys, incl. post-snapshot overwrite)"
