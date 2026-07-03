#!/bin/bash
# v2.7 text-kind gate — RFC D5 clamps against a real server:
#
#   1. IDX.QUERY MATCH p95 < 20ms @ 1M docs (~100 bytes mixed-script
#      text each), median-connection protocol.
#   2. Text memory formula (RFC D4): IDX.VERIFY bytes vs
#      Σ_token(len+48) + postings×24 + docs×32, within ±30%
#      (the formula IS the measured stats shape — this clamp guards
#      the ratio of formula-to-RSS-delta instead: formula must
#      explain ≥ half and ≤ 1.5× of the real growth).
#
# Usage: bash bench/textgate.sh <kevy-binary>
set -u
BIN=${1:?usage: textgate.sh <kevy-binary>}
PORT=7052
DIR=$(mktemp -d /tmp/kevy-textgate-XXXXXX)

PIN=""
command -v taskset >/dev/null 2>&1 && PIN="taskset -c 0-7"
CLIENT_PIN=""
command -v taskset >/dev/null 2>&1 && CLIENT_PIN="taskset -c 8-15"
env KEVY_BIND=127.0.0.1 $PIN "$BIN" --threads 8 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SRV=$!
sleep 1.2

$CLIENT_PIN python3 - "$PORT" "$SRV" <<'PYEOF'
import socket, sys, time

port = int(sys.argv[1])
srv_pid = int(sys.argv[2])

def connect():
    s = socket.create_connection(("127.0.0.1", port))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return s

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
    raise RuntimeError(l)

def cmd(sock, buf, *parts):
    sock.sendall(enc(*parts))
    return read_reply(sock, buf)

def rss_kb():
    with open(f"/proc/{srv_pid}/status") as f:
        for ln in f:
            if ln.startswith("VmRSS:"):
                return int(ln.split()[1])
    return 0

# Zipfian-ish vocabulary (rank r keyword appears with frequency ~1/r):
# a 14-word corpus is degenerate — every term hits ~40% of docs and NO
# ranking engine can prune a uniform corpus. Real text is Zipfian.
import random
random.seed(7)
VOCAB = [f"w{r}" for r in range(10_000)]
def pick():
    # inverse-CDF Zipf-ish sample over ranks
    u = random.random()
    return VOCAB[min(int(10_000 ** u) - 1, 9_999)]
CJK = "全文检索引擎支持中文分词高速缓存数据结构性能优化内存管理并发网络协议解析持久化复制订阅频道集群拓扑"
N = 1_000_000
s = connect(); buf = [b""]
t0 = time.time()
batch = []
for i in range(N):
    w = " ".join(pick() for _ in range(10))
    c0 = (i * 7) % (len(CJK) - 6)
    body = f"{w} {CJK[c0:c0+6]} doc{i}"
    batch.append(enc("HSET", f"a:{i}", "body", body))
    if len(batch) == 2000:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            read_reply(s, buf)
        batch = []
if batch:
    s.sendall(b"".join(batch))
    for _ in range(len(batch)):
        read_reply(s, buf)
print(f"textgate: loaded {N} docs in {time.time()-t0:.1f}s")

rss_before = rss_kb()
r = cmd(s, buf, "IDX.CREATE", "a_body", "ON", "PREFIX", "a:", "FIELD", "body", "TYPE", "str", "KIND", "text")
assert r == b"+OK", r
t0 = time.time()
while True:
    r = cmd(s, buf, "IDX.QUERY", "a_body", "MATCH", "rust", "LIMIT", "1")
    if isinstance(r, list):
        break
    if time.time() - t0 > 600:
        print("textgate: FAIL — build timeout"); sys.exit(1)
    time.sleep(0.5)
print(f"textgate: text index built in {time.time()-t0:.1f}s")

# ---- clamp 1: MATCH p95 median-conn < 20ms ----
# query mix: head terms (w0/w1 ~ in most docs), mid, tail, CJK
queries = [("w0 w1 w512",), ("检索引擎",), ("w3 w800 w4000",),
           ("性能优化",), ("w0 w9000",)]
p95s = []
for _ in range(6):
    c = connect(); cb = [b""]
    lat = []
    for i in range(100):
        q = queries[i % len(queries)][0]
        t = time.time()
        r = cmd(c, cb, "IDX.QUERY", "a_body", "MATCH", q, "LIMIT", "10")
        lat.append(time.time() - t)
        assert isinstance(r, list) and len(r) > 0, r
    lat.sort()
    p95s.append(lat[94] * 1000)
    c.close()
p95s.sort()
print(f"textgate: MATCH p95 per-conn median={p95s[3]:.2f}ms worst={p95s[5]:.2f}ms")
if p95s[3] >= 20.0:
    print(f"textgate: FAIL — MATCH median-conn p95 {p95s[3]:.2f}ms >= 20ms"); sys.exit(1)

# ---- clamp 2: memory formula vs RSS growth ----
r = cmd(s, buf, "IDX.VERIFY", "a_body")
kv = {r[i].decode(): r[i+1].decode() for i in range(0, len(r), 2)}
formula = int(kv["bytes"])
docs = int(kv["entries"])
assert docs == N, kv
rss_after = rss_kb()
growth = (rss_after - rss_before) * 1024
ratio = formula / growth if growth > 0 else 0
print(f"textgate: formula={formula/2**20:.0f}MiB rss-growth={growth/2**20:.0f}MiB ratio={ratio:.2f}")
if not (0.5 <= ratio <= 1.5):
    print(f"textgate: FAIL — formula explains {ratio:.2f}x of RSS growth (want 0.5-1.5)"); sys.exit(1)
print("textgate: PASS")
PYEOF
RC=$?
kill $SRV 2>/dev/null
rm -rf "$DIR"
exit $RC
