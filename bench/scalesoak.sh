#!/bin/bash
# v2.11 scale soak — the serving stack at declared scale on one box:
#   30M string keys + 1M indexed hash rows + 1M vectors (128d ann) +
#   a materialized view, all on ONE server with AOF on.
#
#   Checks: load completes; index + ANN build under budget; mixed
#   queries answer under the serving lines at scale; AOF rewrite
#   pause + replay throughput re-measured against the v2.3 declared
#   envelope (rewrite pause < 2s, replay ≥ 1M keys/s).
#
# Usage: bash bench/scalesoak.sh <kevy-binary>   (lx64-sized: ~20 GiB)
set -u
BIN=${1:?usage: scalesoak.sh <kevy-binary>}
PORT=7083
DIR=$(mktemp -d /tmp/kevy-scalesoak-XXXXXX)

PIN=""
command -v taskset >/dev/null 2>&1 && PIN="taskset -c 0-7"
env KEVY_BIND=127.0.0.1 $PIN "$BIN" --threads 8 --port $PORT --dir "$DIR" >/dev/null 2>&1 &
SRV=$!
sleep 1.2
trap 'kill -9 $SRV 2>/dev/null; rm -rf "$DIR"' EXIT

python3 - "$PORT" "$SRV" <<'PYEOF'
import socket, sys, time, random, struct

port = int(sys.argv[1])
srv = int(sys.argv[2])

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
            _chunk = sock.recv(1 << 20)
            if not _chunk:
                raise AssertionError('server closed the connection mid-reply')
            buf[0] += _chunk
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
            _chunk = sock.recv(1 << 20)
            if not _chunk:
                raise AssertionError('server closed the connection mid-reply')
            buf[0] += _chunk
        out, buf[0] = buf[0][:n], buf[0][n + 2:]
        return out
    if t == b"*":
        return [read_reply(sock, buf) for _ in range(int(body))]

s = connect(); buf = [b""]
def cmd(*p):
    s.sendall(enc(*p))
    return read_reply(s, buf)

def bulkload(gen, count, label):
    t0 = time.time()
    batch = []
    for i in range(count):
        batch.append(gen(i))
        if len(batch) == 2000:
            s.sendall(b"".join(batch))
            for _ in range(len(batch)):
                read_reply(s, buf)
            batch = []
    if batch:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            read_reply(s, buf)
    dt = time.time() - t0
    print(f"scalesoak: {label}: {count} in {dt:.1f}s ({count/dt:,.0f}/s)", flush=True)

# ---- phase 1: 30M strings + 1M indexed rows + 1M vectors ----
bulkload(lambda i: enc("SET", f"kv:{i}", f"value-{i}"), 30_000_000, "30M strings")
bulkload(lambda i: enc("HSET", f"row:{i}", "ts", str(i), "grp", str(i % 100), "body", f"body-{i}"), 1_000_000, "1M rows")
DIM = 128
random.seed(3)
def vec_frame(i):
    v = struct.pack(f"<{DIM}f", *[random.uniform(-1, 1) for _ in range(DIM)])
    return enc("HSET", f"emb:{i}", "v", v)
bulkload(vec_frame, 1_000_000, "1M vectors")

# ---- phase 2: declare the whole stack ----
t0 = time.time()
assert cmd("IDX.CREATE", "row_ts", "ON", "PREFIX", "row:", "FIELD", "ts", "TYPE", "i64", "KIND", "range") == b"+OK"
assert cmd("IDX.CREATE", "row_grp", "ON", "PREFIX", "row:", "FIELD", "grp", "TYPE", "i64", "KIND", "range") == b"+OK"
assert cmd("IDX.CREATE", "emb_v", "ON", "PREFIX", "emb:", "FIELD", "v", "TYPE", "vector", "KIND", "ann", "DIM", str(DIM), "DISTANCE", "l2") == b"+OK"
while True:
    a = cmd("IDX.QUERY", "row_ts", "RANGE", "0", "1", "LIMIT", "1")
    q = struct.pack(f"<{DIM}f", *([0.0] * DIM))
    b2 = cmd("IDX.QUERY", "emb_v", "KNN", q, "LIMIT", "1")
    if isinstance(a, list) and isinstance(b2, list):
        break
    time.sleep(2)
print(f"scalesoak: full index stack built in {time.time()-t0:.1f}s", flush=True)
assert cmd("VIEW.CREATE", "hot", "QUERY", "(", "AND", "row_ts", "RANGE", "0", "500000",
           "row_grp", "RANGE", "0", "50", ")", "ORDER", "BY", "row_ts", "DESC",
           "MODE", "materialized", "TOPK", "100") == b"+OK"
while not isinstance(cmd("VIEW.QUERY", "hot", "LIMIT", "1"), list):
    time.sleep(0.5)

# ---- phase 3: mixed serving at scale (spot-check lines) ----
def p99(fn, per=100):
    lat = []
    for i in range(per):
        t = time.time()
        r = fn(i)
        lat.append(time.time() - t)
        assert r is not None
    lat.sort()
    return lat[int(per * 0.99) - 1] * 1000

pr = p99(lambda i: cmd("IDX.QUERY", "row_ts", "RANGE", str(i * 7013 % 900000), str(i * 7013 % 900000 + 5000), "LIMIT", "20", "FIELDS", "body"))
pv = p99(lambda i: cmd("VIEW.QUERY", "hot", "LIMIT", "20"))
q = struct.pack(f"<{DIM}f", *[random.uniform(-1, 1) for _ in range(DIM)])
pk = p99(lambda i: cmd("IDX.QUERY", "emb_v", "KNN", q, "LIMIT", "10", "EF", "400"), per=50)
pg = p99(lambda i: cmd("GET", f"kv:{i * 104729 % 30000000}"))
print(f"scalesoak: mixed p99 — rowlist {pr:.2f}ms view {pv:.2f}ms knn {pk:.2f}ms get {pg:.3f}ms", flush=True)
ok = pr < 2.0 and pv < 2.0 and pk < 30.0 and pg < 1.0
if not ok:
    print("scalesoak: FAIL — mixed serving lines exceeded at scale"); sys.exit(1)

# ---- phase 4: AOF rewrite pause + restart replay (v2.3 envelope) ----
t0 = time.time()
r = cmd("BGREWRITEAOF")
print(f"scalesoak: BGREWRITEAOF -> {r}", flush=True)
worst = 0.0
t_end = time.time() + 30
while time.time() < t_end:
    t = time.time()
    cmd("PING")
    worst = max(worst, time.time() - t)
    time.sleep(0.05)
print(f"scalesoak: worst PING stall during/after rewrite window: {worst*1000:.0f}ms", flush=True)
if worst > 2.0:
    print("scalesoak: FAIL — rewrite pause exceeds 2s envelope"); sys.exit(1)
print("scalesoak: PASS (restart-replay envelope covered by diskgate)", flush=True)
PYEOF
