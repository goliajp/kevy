#!/bin/bash
# v2.6 view-engine gate — the RFC §5 clamps against a real server:
#
#   1. Virtual VIEW.QUERY p99 < 3ms @ 1M rows × 2 components
#      (median-connection protocol, same as idxgate).
#   2. Materialized VIEW.QUERY p99 < 2ms (the index-read line).
#   3. WRITE TAX: HSET throughput with 3 indexes + 4 materialized
#      TOP-K views (the RFC's hot-list shape; K=100) < 15% below the
#      same workload with 3 indexes only. Unbounded materialized
#      views inherently pay O(log members) per write — documented in
#      docs/views.md, not a gate line.
#   4. View memory formula: VERIFY bytes/member within ±20% of
#      order_value(8) + avg_key_len + 48.
#
# Usage: bash bench/viewgate.sh <kevy-binary>
set -u
BIN=${1:?usage: viewgate.sh <kevy-binary>}
PORT=7051
DIR=$(mktemp -d /tmp/kevy-viewgate-XXXXXX)

PIN=""
command -v taskset >/dev/null 2>&1 && PIN="taskset -c 0-7"
CLIENT_PIN=""
command -v taskset >/dev/null 2>&1 && CLIENT_PIN="taskset -c 8-15"
env KEVY_BIND=127.0.0.1 $PIN "$BIN" --threads 8 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SRV=$!
sleep 1.2

$CLIENT_PIN python3 - "$PORT" <<'PYEOF'
import socket, sys, time, random

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

def load_rows(s, buf, n, start=0):
    batch = []
    for i in range(start, start + n):
        batch.append(enc("HSET", f"r:{i}", "ts", str(i), "grp", str(i % 100), "st", "a" if i % 4 else "b"))
        if len(batch) == 2000:
            s.sendall(b"".join(batch))
            for _ in range(len(batch)):
                read_reply(s, buf)
            batch = []
    if batch:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            read_reply(s, buf)

def wait_ready(s, buf, probe):
    t0 = time.time()
    while True:
        r = cmd(s, buf, *probe)
        if not (isinstance(r, bytes) and r.startswith(b"-INDEXBUILDING")):
            return
        if time.time() - t0 > 300:
            print("viewgate: FAIL — build timeout"); sys.exit(1)
        time.sleep(0.2)

N = 1_000_000
s = connect(); buf = [b""]
t0 = time.time()
load_rows(s, buf, N)
print(f"viewgate: loaded {N} rows in {time.time()-t0:.1f}s")

for spec in (("r_ts", "ts", "i64"), ("r_grp", "grp", "i64"), ("r_st", "st", "str")):
    r = cmd(s, buf, "IDX.CREATE", spec[0], "ON", "PREFIX", "r:", "FIELD", spec[1], "TYPE", spec[2], "KIND", "range")
    assert r == b"+OK", r
wait_ready(s, buf, ("IDX.QUERY", "r_ts", "RANGE", "0", "1", "LIMIT", "1"))

# ---- clamp 1: virtual view (2 components) p99 < 3ms, median conn ----
r = cmd(s, buf, "VIEW.CREATE", "v_virt", "QUERY", "(", "AND",
        "r_ts", "RANGE", "0", "500000", "r_st", "EQ", "b", ")",
        "ORDER", "BY", "r_ts")
assert r == b"+OK", r
wait_ready(s, buf, ("VIEW.QUERY", "v_virt", "LIMIT", "1"))
def conn_p99s(query, conns=6, per=120):
    out = []
    for _ in range(conns):
        c = connect(); cb = [b""]
        lat = []
        for _ in range(per):
            t = time.time()
            r = cmd(c, cb, *query)
            lat.append(time.time() - t)
            assert isinstance(r, list), r
        lat.sort()
        out.append(lat[int(per * 0.99) - 1] * 1000)
        c.close()
    out.sort()
    return out
# NB: virtual eval materializes the full member set per query — the
# clamp keeps that honest.
p = conn_p99s(("VIEW.QUERY", "v_virt", "LIMIT", "100"))
print(f"viewgate: virtual p99 per-conn median={p[3]:.2f}ms worst={p[5]:.2f}ms")
if p[3] >= 3.0:
    print(f"viewgate: FAIL — virtual median-conn p99 {p[3]:.2f}ms >= 3ms"); sys.exit(1)

# ---- clamp 2: materialized read p99 < 2ms ----
r = cmd(s, buf, "VIEW.CREATE", "v_mat", "QUERY", "(", "AND",
        "r_ts", "RANGE", "0", "500000", "r_st", "EQ", "b", ")",
        "ORDER", "BY", "r_ts", "MODE", "materialized")
assert r == b"+OK", r
wait_ready(s, buf, ("VIEW.QUERY", "v_mat", "LIMIT", "1"))
p = conn_p99s(("VIEW.QUERY", "v_mat", "LIMIT", "100"))
print(f"viewgate: materialized p99 per-conn median={p[3]:.2f}ms worst={p[5]:.2f}ms")
if p[3] >= 2.0:
    print(f"viewgate: FAIL — materialized median-conn p99 {p[3]:.2f}ms >= 2ms"); sys.exit(1)

# ---- clamp 4: memory formula (VERIFY bytes/member) ----
r = cmd(s, buf, "VIEW.VERIFY", "v_mat")
kv = {r[i].decode(): r[i+1].decode() for i in range(0, len(r), 2)}
members, measured = int(kv["members"]), int(kv["bytes"])
assert members > 100_000, kv
avg_key = sum(len(f"r:{i}") for i in range(0, N, 100_000)) / 10
formula = members * (8 + avg_key + 48)
ratio = measured / formula
print(f"viewgate: view bytes/member measured={measured/members:.1f} formula={formula/members:.1f} ratio={ratio:.2f}")
if not (0.8 <= ratio <= 1.2):
    print(f"viewgate: FAIL — view memory formula off by {ratio:.2f}x"); sys.exit(1)

# ---- clamp 3: write tax — 3 idx + 4 materialized views vs 3 idx ----
# Baseline first: drop the views, measure HSET update throughput.
def write_burst(rounds=40_000):
    t0 = time.time()
    batch = []
    for i in range(rounds):
        k = random.randrange(0, N)
        batch.append(enc("HSET", f"r:{k}", "ts", str(random.randrange(0, N))))
        if len(batch) == 1000:
            s.sendall(b"".join(batch))
            for _ in range(len(batch)):
                read_reply(s, buf)
            batch = []
    if batch:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            read_reply(s, buf)
    return rounds / (time.time() - t0)

cmd(s, buf, "VIEW.DROP", "v_mat")
cmd(s, buf, "VIEW.DROP", "v_virt")
# Paired alternation ×3 (successive bursts drift regardless of views —
# an A-then-B protocol misread the drift as a 45%→27% "tax"; medians
# of interleaved pairs cancel it).
def make_views():
    for i in range(4):
        r = cmd(s, buf, "VIEW.CREATE", f"v_tax{i}", "QUERY", "(", "AND",
                "r_ts", "RANGE", "0", str(250_000 * (i + 1)), "r_grp", "RANGE", "0", str(25 * (i + 1)), ")",
                "ORDER", "BY", "r_ts", "MODE", "materialized", "TOPK", "100")
        assert r == b"+OK", r
    wait_ready(s, buf, ("VIEW.QUERY", "v_tax3", "LIMIT", "1"))

def drop_views():
    for i in range(4):
        cmd(s, buf, "VIEW.DROP", f"v_tax{i}")

_ = write_burst()  # warm
bases, taxeds = [], []
for _round in range(3):
    bases.append(write_burst())
    make_views()
    _ = write_burst()  # settle: post-create transient decays over ~one burst
    taxeds.append(write_burst())
    drop_views()
bases.sort(); taxeds.sort()
base, taxed = bases[1], taxeds[1]
tax = (base - taxed) / base * 100
print(f"viewgate: write tax bases={[int(b) for b in bases]} taxed={[int(t) for t in taxeds]} median tax={tax:.1f}%")
if tax >= 15.0:
    print(f"viewgate: FAIL — write tax {tax:.1f}% >= 15%"); sys.exit(1)
print("viewgate: PASS")
PYEOF
RC=$?
kill $SRV 2>/dev/null
rm -rf "$DIR"
exit $RC
