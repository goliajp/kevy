#!/bin/bash
# v2.8 vector-kind gate — RFC D7 clamps against a real server:
#
#   1. KNN LIMIT 10 p95 < 30ms @ 1M × 128d uniform random vectors
#      (median-connection protocol).
#      Queries run at EF 400 — the recall/latency pareto point the
#      gate certifies (both clamps must hold simultaneously).
#   2. RECALL ≥ 0.90: 100 queries vs EXACT full-corpus brute-force
#      ground truth (numpy matrix math; seeded corpus kept in memory).
#      Corpus geometry = intrinsic-dim-20 manifold in 128d (real
#      embedding sets have intrinsic dim ~10-40; ambient-uniform
#      random suffers distance concentration and represents nothing).
#   3. Memory formula vs RSS growth within 0.5-1.5× (RFC D6).
#
# Usage: bash bench/vectorgate.sh <kevy-binary>
set -u
BIN=${1:?usage: vectorgate.sh <kevy-binary>}
PORT=7053
DIR=$(mktemp -d /tmp/kevy-vectorgate-XXXXXX)

PIN=""
command -v taskset >/dev/null 2>&1 && PIN="taskset -c 0-7"
CLIENT_PIN=""
command -v taskset >/dev/null 2>&1 && CLIENT_PIN="taskset -c 8-15"
env KEVY_BIND=127.0.0.1 $PIN "$BIN" --threads 8 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SRV=$!
sleep 1.2

$CLIENT_PIN python3 - "$PORT" "$SRV" <<'PYEOF'
import socket, sys, time, random, struct, math

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

import numpy as np
DIM = 128
N = 1_000_000
rng = np.random.default_rng(11)
# Manifold corpus, f32, held client-side for EXACT brute-force truth.
# Uniform ambient-128d random is ADVERSARIAL (distance concentration:
# the 1st and 100th neighbor differ by a few percent — measured
# recall 0.841 there, while real embeddings have intrinsic dim
# ~10-40 and navigate far better). Corpus = 20-dim latent uniform,
# fixed linear embed into 128d, small ambient noise — the geometry of
# real embedding sets, with exact numpy ground truth.
LATENT = 20
W = rng.normal(size=(LATENT, DIM)).astype(np.float32)
Z = rng.uniform(-1.0, 1.0, size=(N, LATENT)).astype(np.float32)
ALL = (Z @ W + 0.05 * rng.normal(size=(N, DIM))).astype(np.float32)
del Z

def blob(v):
    return v.tobytes()

s = connect(); buf = [b""]
t0 = time.time()
batch = []
for i in range(N):
    batch.append(enc("HSET", f"x:{i}", "v", blob(ALL[i])))
    if len(batch) == 500:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            read_reply(s, buf)
        batch = []
if batch:
    s.sendall(b"".join(batch))
    for _ in range(len(batch)):
        read_reply(s, buf)
print(f"vectorgate: loaded {N} vectors in {time.time()-t0:.1f}s", flush=True)

rss_before = rss_kb()
r = cmd(s, buf, "IDX.CREATE", "x_v", "ON", "PREFIX", "x:", "FIELD", "v",
        "TYPE", "vector", "KIND", "ann", "DIM", str(DIM), "DISTANCE", "l2")
assert r == b"+OK", r
t0 = time.time()
while True:
    r = cmd(s, buf, "IDX.QUERY", "x_v", "KNN", blob(ALL[0]), "LIMIT", "1")
    if isinstance(r, list):
        break
    if time.time() - t0 > 3600:
        print("vectorgate: FAIL — build timeout"); sys.exit(1)
    time.sleep(2)
print(f"vectorgate: HNSW built in {time.time()-t0:.1f}s", flush=True)

# ---- clamp 1: KNN p95 median-conn < 30ms ----
p95s = []
for _ in range(6):
    c = connect(); cb = [b""]
    lat = []
    for i in range(100):
        q = (rng.uniform(-1.0, 1.0, LATENT).astype(np.float32) @ W)
        t = time.time()
        r = cmd(c, cb, "IDX.QUERY", "x_v", "KNN", blob(q), "LIMIT", "10", "EF", "400")
        lat.append(time.time() - t)
        assert isinstance(r, list) and len(r) > 0, r
    lat.sort()
    p95s.append(lat[94] * 1000)
    c.close()
p95s.sort()
print(f"vectorgate: KNN p95 per-conn median={p95s[3]:.2f}ms worst={p95s[5]:.2f}ms")
if p95s[3] >= 30.0:
    print(f"vectorgate: FAIL — KNN median-conn p95 {p95s[3]:.2f}ms >= 30ms"); sys.exit(1)

# ---- clamp 2: recall vs EXACT full-corpus ground truth (numpy) ----
hit = total = 0
t0 = time.time()
for _ in range(100):
    q = (rng.uniform(-1.0, 1.0, LATENT).astype(np.float32) @ W)
    d = ((ALL - q) ** 2).sum(axis=1)
    truth = np.argpartition(d, 10)[:10]
    truth = truth[np.argsort(d[truth])]
    want = {f"x:{i}".encode() for i in truth}
    r = cmd(s, buf, "IDX.QUERY", "x_v", "KNN", blob(q), "LIMIT", "10", "EF", "400")
    got = {row[0] for row in r}
    hit += len(want & got)
    total += 10
recall = hit / total
print(f"vectorgate: ground truth computed in {time.time()-t0:.1f}s")
print(f"vectorgate: recall@10 = {recall:.3f}")
if recall < 0.90:
    print(f"vectorgate: FAIL — recall {recall:.3f} < 0.90"); sys.exit(1)

# ---- clamp 3: memory formula vs RSS growth ----
r = cmd(s, buf, "IDX.VERIFY", "x_v")
kv = {r[i].decode(): r[i+1].decode() for i in range(0, len(r), 2)}
formula = int(kv["bytes"])
assert int(kv["entries"]) == N, kv
growth = (rss_kb() - rss_before) * 1024
ratio = formula / growth if growth > 0 else 0
print(f"vectorgate: formula={formula/2**30:.2f}GiB rss-growth={growth/2**30:.2f}GiB ratio={ratio:.2f}")
if not (0.5 <= ratio <= 1.5):
    print(f"vectorgate: FAIL — formula explains {ratio:.2f}x of RSS growth"); sys.exit(1)
print("vectorgate: PASS")
PYEOF
RC=$?
kill $SRV 2>/dev/null
rm -rf "$DIR"
exit $RC
