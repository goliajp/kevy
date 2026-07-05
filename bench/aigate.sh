#!/usr/bin/env bash
# aigate — the AI-friendliness contract gate (v3.10 phase 1).
#
# An agent with zero out-of-band knowledge must be able to: discover
# every verb + its syntax (COMMAND DOCS), recover from extension
# errors in-band (self-explaining -ERR), read a query plan
# (IDX.EXPLAIN), and get typed maps on RESP3. The bidirectional
# VERB_META ↔ dispatch parity lives in cargo tests
# (tests_verb_meta.rs); this gate exercises the LIVE wire.
#
# Usage: bash bench/aigate.sh <kevy-binary>
set -u
KBIN=${1:?usage: aigate.sh <kevy-binary>}
KBIN=$(cd "$(dirname "$KBIN")" && pwd)/$(basename "$KBIN")
cd "$(dirname "$0")/.."
PORT=7331
DIR=$(mktemp -d /tmp/kevy-aigate-XXXXXX)
SPID=""
trap 'kill $SPID 2>/dev/null; rm -rf "$DIR"' EXIT

env KEVY_BIND=127.0.0.1 "$KBIN" --threads 4 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SPID=$!
sleep 1.5

python3 - $PORT <<'PY'
import socket, sys, time

port = int(sys.argv[1])

def conn():
    s = socket.create_connection(("127.0.0.1", port))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return s, [b""]

def enc(*p):
    b = b"*%d\r\n" % len(p)
    for x in p:
        x = x.encode() if isinstance(x, str) else x
        b += b"$%d\r\n%s\r\n" % (len(x), x)
    return b

def rd(s, buf):
    def line():
        while b"\r\n" not in buf[0]:
            buf[0] += s.recv(1 << 20)
        l, _, r = buf[0].partition(b"\r\n"); buf[0] = r; return l
    l = line(); t, body = l[:1], l[1:]
    if t in (b"+", b"-", b":"): return l
    if t == b"$":
        n = int(body)
        if n < 0: return None
        while len(buf[0]) < n + 2: buf[0] += s.recv(1 << 20)
        out = buf[0][:n]; buf[0] = buf[0][n+2:]; return out
    if t in (b"*", b"%"):
        cnt = int(body) * (2 if t == b"%" else 1)
        return [rd(s, buf) for _ in range(cnt)]
    raise RuntimeError(l)

def cmd(s, buf, *p):
    s.sendall(enc(*p)); return rd(s, buf)

fails = []
def clamp(name, ok, detail=""):
    print(f"aigate: {'ok' if ok else 'FAIL'} — {name}" + (f" ({detail})" if detail else ""))
    if not ok:
        fails.append(name)

s, buf = conn()

# ---- clamp 1: discovery — COMMAND COUNT/LIST/DOCS agree and are rich
count = int(cmd(s, buf, "COMMAND", "COUNT")[1:])
names = cmd(s, buf, "COMMAND", "LIST")
clamp("COMMAND COUNT == len(LIST)", count == len(names) and count >= 180, f"count={count}")
docs = cmd(s, buf, "COMMAND", "DOCS")
clamp("DOCS covers every LIST verb", len(docs) == 2 * count)
# every extension verb documents itself with a syntax that names it
ext = [n.decode() for n in names if b"." in n]
clamp("extension verbs present in LIST", len(ext) >= 20, f"n={len(ext)}")
d = cmd(s, buf, "COMMAND", "DOCS", "IDX.QUERY")
fields = {d[1][i]: d[1][i+1] for i in range(0, len(d[1]) - 1, 2) if isinstance(d[1][i], bytes)}
clamp("IDX.QUERY DOCS has syntax+summary+flags",
      b"syntax" in fields and b"summary" in fields and fields[b"syntax"].startswith(b"IDX.QUERY"))

# ---- clamp 2: self-explaining extension errors
for i in range(50):
    cmd(s, buf, "HSET", f"g:{i}", "v", str(i))
cmd(s, buf, "IDX.CREATE", "gi", "ON", "PREFIX", "g:", "FIELD", "v", "TYPE", "i64", "KIND", "range")
t0 = time.time()
while time.time() - t0 < 30:
    r = cmd(s, buf, "IDX.QUERY", "gi", "RANGE", "0", "9", "LIMIT", "1")
    if isinstance(r, list):
        break
    time.sleep(0.3)
e1 = cmd(s, buf, "IDX.QUERY", "nope", "RANGE", "0", "1")
clamp("no-such-index error names index + recovery",
      e1.startswith(b"-ERR no such index 'nope'") and b"IDX.LIST" in e1, e1[:60].decode())
e2 = cmd(s, buf, "IDX.EXPLAIN", "gi", "RANGE")
clamp("bad-args error names verb + points at DOCS",
      e2.startswith(b"-ERR IDX.EXPLAIN 'gi'") and b"COMMAND DOCS" in e2, e2[:60].decode())

# ---- clamp 3: IDX.EXPLAIN structure + est_rows sanity
ex = cmd(s, buf, "IDX.EXPLAIN", "gi", "RANGE", "0", "49", "LIMIT", "10")
kv = {p[0]: p[1] for p in ex if isinstance(p, list) and len(p) == 2}
est = int(kv.get(b"est_rows", b"0"))
clamp("EXPLAIN pairs: kind/state/est_rows/plan",
      kv.get(b"kind") == b"range" and kv.get(b"state") == b"ready" and b"plan" in kv)
clamp("est_rows within 10x of truth", 5 <= est <= 500, f"est={est}")

# ---- clamp 4: RESP3 maps
s3, buf3 = conn()
h = cmd(s3, buf3, "HELLO", "3")
clamp("HELLO 3 ack is a map with proto=3",
      isinstance(h, list) and b"proto" in h and h[h.index(b"proto") + 1] == b":3")
raw = enc("IDX.EXPLAIN", "gi", "RANGE", "0", "9")
s3.sendall(raw)
time.sleep(0.3)
first = s3.recv(1)
clamp("EXPLAIN on RESP3 conn is a Map (%)", first == b"%")

print("aigate: PASS" if not fails else f"aigate: FAIL — {fails}")
sys.exit(1 if fails else 0)
PY
RC=$?
exit $RC
