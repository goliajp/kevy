#!/bin/bash
# v2.5 index-engine gate — the RFC's two perf clamps + the D7 memory
# formula, measured against a real server:
#
#   1. IDX.QUERY latency: p99 < 2ms over 200 queries against a
#      1M-row i64 range index (LIMIT 100 pages at random offsets).
#   2. Memory formula: measured bytes/row within ±20% of
#      value(8) + avg_key_len + 48.
#
# (Clamp #0 — empty-catalog 0% write regression — is perfgate itself:
# its 7 angles run with no catalog declared.)
#
# Usage: bash bench/idxgate.sh <kevy-binary>
set -u
BIN=${1:?usage: idxgate.sh <kevy-binary>}
PORT=7041
DIR=$(mktemp -d /tmp/kevy-idxgate-XXXXXX)
fail() { echo "idxgate: FAIL — $1" >&2; kill $SRV 2>/dev/null; rm -rf "$DIR"; exit 1; }

PIN=""
command -v taskset >/dev/null 2>&1 && PIN="taskset -c 0-7"
env KEVY_BIND=127.0.0.1 $PIN "$BIN" --threads 8 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SRV=$!
sleep 1.2

python3 - "$PORT" <<'PYEOF' || { kill $SRV 2>/dev/null; rm -rf "$DIR"; echo "idxgate: FAIL" >&2; exit 1; }
import socket, sys, time

port = int(sys.argv[1])

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

s = connect(); buf = [b""]

# ---- load 1M rows (pipelined) ----
N = 1_000_000
t0 = time.time()
batch = []
for i in range(N):
    batch.append(enc("HSET", f"g:{i}", "ts", str(i)))
    if len(batch) == 2000:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            read_reply(s, buf)
        batch = []
if batch:
    s.sendall(b"".join(batch))
    for _ in range(len(batch)):
        read_reply(s, buf)
print(f"idxgate: loaded {N} rows in {time.time()-t0:.1f}s")

# ---- build index, wait ready ----
r = cmd(s, buf, "IDX.CREATE", "g_ts", "ON", "PREFIX", "g:", "FIELD", "ts", "TYPE", "i64", "KIND", "range")
assert r == b"+OK", r
t0 = time.time()
while True:
    r = cmd(s, buf, "IDX.QUERY", "g_ts", "RANGE", "0", "10", "LIMIT", "1")
    if not (isinstance(r, bytes) and r.startswith(b"-INDEXBUILDING")):
        break
    if time.time() - t0 > 300:
        print("idxgate: build timed out"); sys.exit(1)
    time.sleep(0.2)
print(f"idxgate: 1M-row build ready in {time.time()-t0:.1f}s")

# ---- clamp 1: query p99 < 2ms ----
import random
lat = []
for _ in range(200):
    lo = random.randrange(0, N - 20_000)
    t = time.time()
    r = cmd(s, buf, "IDX.QUERY", "g_ts", "RANGE", str(lo), str(lo + 20_000), "LIMIT", "100")
    lat.append(time.time() - t)
    assert isinstance(r, list) and len(r) == 2, r
lat.sort()
p50, p99 = lat[100] * 1000, lat[197] * 1000
print(f"idxgate: IDX.QUERY p50={p50:.2f}ms p99={p99:.2f}ms")
if p99 >= 2.0:
    print(f"idxgate: FAIL — p99 {p99:.2f}ms >= 2ms"); sys.exit(1)

# ---- clamp 2: memory formula ±20% ----
r = cmd(s, buf, "IDX.VERIFY", "g_ts")
kv = {r[i].decode(): r[i+1].decode() for i in range(0, len(r), 2)}
entries, measured = int(kv["entries"]), int(kv["bytes"])
assert entries == N, kv
avg_key = sum(len(f"g:{i}") for i in range(0, N, 100_000)) / 10
formula = N * (8 + avg_key + 48)
ratio = measured / formula
print(f"idxgate: bytes/row measured={measured/N:.1f} formula={formula/N:.1f} ratio={ratio:.2f}")
if not (0.8 <= ratio <= 1.2):
    print(f"idxgate: FAIL — memory formula off by {ratio:.2f}x"); sys.exit(1)
print("idxgate: PASS")
PYEOF
RC=$?
kill $SRV 2>/dev/null
rm -rf "$DIR"
exit $RC
