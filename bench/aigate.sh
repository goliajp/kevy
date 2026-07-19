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
            _chunk = s.recv(1 << 20)
            if not _chunk:
                raise AssertionError('server closed the connection mid-reply')
            buf[0] += _chunk
        l, _, r = buf[0].partition(b"\r\n"); buf[0] = r; return l
    l = line(); t, body = l[:1], l[1:]
    if t in (b"+", b"-", b":"): return l
    if t == b"$":
        n = int(body)
        if n < 0: return None
        while len(buf[0]) < n + 2:
            _chunk = s.recv(1 << 20)
            if not _chunk:
                raise AssertionError('server closed the connection mid-reply')
            buf[0] += _chunk
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

# ---- clamp 4: HYBRID fusion quality — RRF must not lose to either leg
import struct, random
random.seed(31)
LAT, DIM = 8, 32
W = [[random.gauss(0, 1) for _ in range(DIM)] for _ in range(LAT)]
def mvec():
    z = [random.uniform(-1, 1) for _ in range(LAT)]
    return struct.pack(f"<{DIM}f", *[sum(z[l]*W[l][d] for l in range(LAT)) for d in range(DIM)])
VOC = [f"h{r}" for r in range(500)]
def word():
    return VOC[min(int(500 ** random.random()) - 1, 499)]
for i in range(2000):
    cmd(s, buf, "HSET", f"hb:{i}", "body", " ".join(word() for _ in range(8)), "v", mvec())
cmd(s, buf, "IDX.CREATE", "hb_t", "ON", "PREFIX", "hb:", "FIELD", "body", "TYPE", "str", "KIND", "text")
cmd(s, buf, "IDX.CREATE", "hb_v", "ON", "PREFIX", "hb:", "FIELD", "v", "TYPE", "vector", "KIND", "ann", "DIM", str(DIM), "DISTANCE", "l2")
t0 = time.time()
while time.time() - t0 < 60:
    r1 = cmd(s, buf, "IDX.QUERY", "hb_t", "MATCH", "h0", "LIMIT", "1")
    r2 = cmd(s, buf, "IDX.QUERY", "hb_v", "KNN", mvec(), "LIMIT", "1")
    if isinstance(r1, list) and isinstance(r2, list):
        break
    time.sleep(0.5)
def keys_of(r):
    return [x[0] for x in r if isinstance(x, list) and x]
# RRF math guarantee (k=60): a DOUBLE-HIT key (top-10 in both legs)
# scores at least 2/(60+10), while any single-hit key at best
# 1/(60+1) — double hits ALWAYS outrank single hits. Clamp exactly
# that provable property.
ok_trials, bad = 0, ""
for t in range(20):
    qtext, qvec = word(), mvec()
    m = keys_of(cmd(s, buf, "IDX.QUERY", "hb_t", "MATCH", qtext, "LIMIT", "10"))
    k = keys_of(cmd(s, buf, "IDX.QUERY", "hb_v", "KNN", qvec, "LIMIT", "10"))
    h = keys_of(cmd(s, buf, "IDX.QUERY", "HYBRID", "hb_t", "MATCH", qtext, "hb_v", "KNN", qvec, "LIMIT", "10"))
    double = set(m) & set(k)
    if len(double) <= 10 and not double.issubset(set(h)):
        bad = f"trial {t}: double-hits {double - set(h)} missing from fusion"
        break
    ok_trials += 1
clamp("double-hit keys always make the RRF top", ok_trials == 20, bad or "20/20")
h2 = cmd(s, buf, "IDX.QUERY", "HYBRID", "hb_t", "MATCH", "h0", "hb_v", "KNN", mvec(), "LIMIT", "5", "RRFK", "10", "FIELDS", "body")
clamp("HYBRID with RRFK + FIELDS hydrates", isinstance(h2, list) and len(h2) == 5 and len(h2[0]) == 4)
e3 = cmd(s, buf, "IDX.QUERY", "HYBRID", "hb_t", "MATCH", "h0", "nope", "KNN", mvec())
clamp("HYBRID missing ann index is self-explaining", e3.startswith(b"-ERR"), e3[:50].decode())

# ---- clamp 5: RESP3 maps
s3, buf3 = conn()
h = cmd(s3, buf3, "HELLO", "3")
clamp("HELLO 3 ack is a map with proto=3",
      isinstance(h, list) and b"proto" in h and h[h.index(b"proto") + 1] == b":3")
raw = enc("IDX.EXPLAIN", "gi", "RANGE", "0", "9")
s3.sendall(raw)
time.sleep(0.3)
first = s3.recv(1)
clamp("EXPLAIN on RESP3 conn is a Map (%)", first == b"%")

print("aigate-wire: PASS" if not fails else f"aigate: FAIL — {fails}")
sys.exit(1 if fails else 0)
PY
RC=$?
[ $RC = 0 ] || exit $RC

# ---- phase 3: MCP session e2e (kevy-mcp binary next to the server binary)
MCP=$(dirname "$KBIN")/kevy-mcp
if [ ! -x "$MCP" ]; then
    echo "aigate: FAIL — kevy-mcp binary missing next to $KBIN"
    exit 1
fi
OUT=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"kevy_read","arguments":{"command":["PING"]}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"kevy_read","arguments":{"command":["SET","a","b"]}}}' \
  '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"kevy_read","arguments":{"command":["BLPOP","x","1"]}}}' \
  | "$MCP" --url redis://127.0.0.1:$PORT 2>/dev/null)
echo "$OUT" > "$DIR/mcp.out"
python3 - "$DIR/mcp.out" <<'PY2'
import json, sys
lines = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
by = {d.get("id"): d for d in lines}
ok = True
def clamp(name, cond):
    global ok
    print(f"aigate-mcp: {'ok' if cond else 'FAIL'} — {name}")
    ok = ok and cond
clamp("initialize returns 2024-11-05", by[1]["result"]["protocolVersion"] == "2024-11-05")
tools = [t["name"] for t in by[2]["result"]["tools"]]
clamp("tools/list has read+discover, no write by default",
      "kevy_read" in tools and "kevy_discover" in tools and "kevy_write" not in tools)
clamp("PING round-trips", "PONG" in json.dumps(by[3].get("result", {})))
clamp("write verb rejected through kevy_read", "error" in by[4] and "write" in by[4]["error"]["message"])
clamp("blocking verb excluded from whitelist", "error" in by[5])
print("aigate: PASS" if ok else "aigate: FAIL — mcp phase")
sys.exit(0 if ok else 1)
PY2
exit $?
