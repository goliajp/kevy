#!/bin/bash
# v2.11 fsck chaos — derived-state honesty after a hard kill:
# with AOF on, run continuous writes over an indexed + viewed
# keyspace, kill -9 the server mid-write, restart, wait for replay +
# backfill, then compare IDX.QUERY / VIEW.QUERY answers against a
# FRESH index/view built from the replayed data (drop + re-create).
# Zero difference = derived-by-construction held across the crash.
#
# Usage: bash bench/chaosfsck.sh <kevy-binary>
set -u
BIN=${1:?usage: chaosfsck.sh <kevy-binary>}
PORT=7082
DIR=$(mktemp -d /tmp/kevy-chaosfsck-XXXXXX)

start_server() {
    env KEVY_BIND=127.0.0.1 "$BIN" --threads 8 --port $PORT --dir "$DIR" >/dev/null 2>&1 &
    SRV=$!
    for _ in $(seq 100); do
        python3 -c "import socket;socket.create_connection((\"127.0.0.1\",$PORT),timeout=0.2)" 2>/dev/null && return
        sleep 0.1
    done
}
start_server
trap 'kill -9 $SRV 2>/dev/null; rm -rf "$DIR"' EXIT

python3 - "$PORT" <<'PYEOF' &
import socket, sys, random
port = int(sys.argv[1])
def enc(*parts):
    buf = b"*%d\r\n" % len(parts)
    for p in parts:
        if isinstance(p, str):
            p = p.encode()
        buf += b"$%d\r\n%s\r\n" % (len(p), p)
    return buf
s = socket.create_connection(("127.0.0.1", port))
buf = [b""]
def rd():
    while b"\r\n" not in buf[0]:
        buf[0] += s.recv(1 << 20)
    l, _, rest = buf[0].partition(b"\r\n")
    buf[0] = rest
    return l
batch = []
for i in range(50_000):
    batch.append(enc("HSET", f"c:{i}", "n", str(i), "flag", str(i % 2)))
    if len(batch) == 500:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            rd()
        batch = []
s.sendall(b"".join(batch))
for _ in range(len(batch)):
    rd()
# declare index + view
def one(*p):
    s.sendall(enc(*p)); return rd()
one("IDX.CREATE", "c_n", "ON", "PREFIX", "c:", "FIELD", "n", "TYPE", "i64", "KIND", "range")
one("VIEW.CREATE", "c_v", "QUERY", "c_n", "RANGE", "0", "40000",
    "ORDER", "BY", "c_n", "DESC", "MODE", "materialized", "TOPK", "50")
# hammer updates forever (until killed with the server)
i = 0
try:
    while True:
        k = random.randrange(0, 50_000)
        one("HSET", f"c:{k}", "n", str(random.randrange(0, 50_000)))
        i += 1
except Exception:
    pass
PYEOF
WRITER=$!
sleep 6   # let load + index build + update hammer run
kill -9 $SRV 2>/dev/null
sleep 0.5
kill $WRITER 2>/dev/null
wait $WRITER 2>/dev/null
echo "chaosfsck: server killed -9 mid-write; restarting"
start_server

python3 - "$PORT" <<'PYEOF'
import socket, sys, time
port = int(sys.argv[1])
def enc(*parts):
    buf = b"*%d\r\n" % len(parts)
    for p in parts:
        if isinstance(p, str):
            p = p.encode()
        buf += b"$%d\r\n%s\r\n" % (len(p), p)
    return buf
def read_reply(sock, buf):
    def line():
        while b"\r\n" not in buf[0]:
            buf[0] += sock.recv(1 << 20)
        l, _, rest = buf[0].partition(b"\r\n")
        buf[0] = rest
        return l
    l = line()
    t, body = l[:1], l[1:]
    if t in (b"+", b"-", b":"):
        return l
    if t == b"$":
        n = int(body)
        if n < 0:
            return None
        while len(buf[0]) < n + 2:
            buf[0] += sock.recv(1 << 20)
        out, buf[0] = buf[0][:n], buf[0][n + 2:]
        return out
    if t == b"*":
        return [read_reply(sock, buf) for _ in range(int(body))]
s = socket.create_connection(("127.0.0.1", port))
buf = [b""]
def cmd(*p):
    s.sendall(enc(*p))
    return read_reply(s, buf)

# wait for replay + backfill (catalog sidecar re-declares the index)
t0 = time.time()
while True:
    r = cmd("IDX.QUERY", "c_n", "RANGE", "0", "1", "LIMIT", "1")
    if isinstance(r, list):
        break
    if time.time() - t0 > 120:
        print("chaosfsck: FAIL — post-crash rebuild timeout"); sys.exit(1)
    time.sleep(0.3)
keys = cmd("DBSIZE")
print(f"chaosfsck: replayed, dbsize={keys}", flush=True)

def snapshot():
    # canonical answers: three range pages + count + view page
    out = []
    for lo, hi in ((0, 5000), (20000, 26000), (39000, 50000)):
        r = cmd("IDX.QUERY", "c_n", "RANGE", str(lo), str(hi), "LIMIT", "1000")
        out.append(repr(r))
        r = cmd("IDX.COUNT", "c_n", "RANGE", str(lo), str(hi))
        out.append(repr(r))
    r = cmd("VIEW.QUERY", "c_v", "LIMIT", "50")
    out.append(repr(r))
    return "\n".join(out)

crash_survivor = snapshot()

# fresh rebuild from replayed data: drop + recreate both
cmd("VIEW.DROP", "c_v")
cmd("IDX.DROP", "c_n")
cmd("IDX.CREATE", "c_n", "ON", "PREFIX", "c:", "FIELD", "n", "TYPE", "i64", "KIND", "range")
t0 = time.time()
while True:
    r = cmd("IDX.QUERY", "c_n", "RANGE", "0", "1", "LIMIT", "1")
    if isinstance(r, list):
        break
    time.sleep(0.3)
cmd("VIEW.CREATE", "c_v", "QUERY", "c_n", "RANGE", "0", "40000",
    "ORDER", "BY", "c_n", "DESC", "MODE", "materialized", "TOPK", "50")
while True:
    r = cmd("VIEW.QUERY", "c_v", "LIMIT", "1")
    if isinstance(r, list):
        break
    time.sleep(0.3)
fresh = snapshot()

if crash_survivor != fresh:
    print("chaosfsck: FAIL — crash-survivor answers differ from fresh rebuild")
    for a, b in zip(crash_survivor.split("\n"), fresh.split("\n")):
        if a != b:
            print(f"  survivor: {a[:120]}")
            print(f"  fresh:    {b[:120]}")
    sys.exit(1)
print("chaosfsck: PASS — crash-survivor index/view identical to fresh rebuild")
PYEOF
