#!/bin/bash
# v3.1 aggregate-kind gate — RFC D5 clamps:
#
#   1. GROUP point query p99 < 1ms @ 1M rows × 10k groups
#      (median of 6 connections).
#   2. GROUPS top-100 (BY sum) p99 < 5ms. Group sizes are
#      Zipf-distributed — real GROUP BY columns (status, tenant,
#      category) are heavy-tailed; a uniform corpus makes every sum a
#      near-tie, which defeats ANY exact pruning
#      information-theoretically (the engine then falls back to full
#      materialization by design).
#   3. WRITE TAX: HSET with one agg index < 10% vs bare (paired
#      alternation ×3, medians — the viewgate protocol).
#   4. Memory formula vs RSS growth 0.5-1.5×.
#
# Usage: bash bench/agggate.sh <kevy-binary>
set -u
BIN=${1:?usage: agggate.sh <kevy-binary>}
PORT=7091
DIR=$(mktemp -d /tmp/kevy-agggate-XXXXXX)

PIN=""
command -v taskset >/dev/null 2>&1 && PIN="taskset -c 0-7"
CLIENT_PIN=""
command -v taskset >/dev/null 2>&1 && CLIENT_PIN="taskset -c 8-15"
env KEVY_BIND=127.0.0.1 $PIN "$BIN" --threads 8 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SRV=$!
sleep 1.2
trap 'kill $SRV 2>/dev/null; rm -rf "$DIR"' EXIT

$CLIENT_PIN python3 - "$PORT" "$SRV" <<'PYEOF'
import socket, sys, time, random

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

def cmd(sock, buf, *parts):
    sock.sendall(enc(*parts))
    return read_reply(sock, buf)

def rss_kb():
    with open(f"/proc/{srv_pid}/status") as f:
        for ln in f:
            if ln.startswith("VmRSS:"):
                return int(ln.split()[1])
    return 0

N = 1_000_000
GROUPS = 10_000
s = connect(); buf = [b""]
t0 = time.time()
batch = []
random.seed(5)
def zipf_group():
    u = random.random()
    return min(int(GROUPS ** u) - 1, GROUPS - 1)
for i in range(N):
    g = f"g{zipf_group()}"
    batch.append(enc("HSET", f"a:{i}", "grp", g, "val", str(random.randrange(1_000_000))))
    if len(batch) == 2000:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            read_reply(s, buf)
        batch = []
if batch:
    s.sendall(b"".join(batch))
    for _ in range(len(batch)):
        read_reply(s, buf)
print(f"agggate: loaded {N} rows / {GROUPS} groups in {time.time()-t0:.1f}s", flush=True)

# ---- write tax (paired alternation ×3, before the index skews RSS) ----
def burst(rounds=40_000):
    t0 = time.time()
    b2 = []
    for i in range(rounds):
        k = random.randrange(N)
        b2.append(enc("HSET", f"a:{k}", "val", str(random.randrange(1_000_000))))
        if len(b2) == 1000:
            s.sendall(b"".join(b2))
            for _ in range(len(b2)):
                read_reply(s, buf)
            b2 = []
    if b2:
        s.sendall(b"".join(b2))
        for _ in range(len(b2)):
            read_reply(s, buf)
    return rounds / (time.time() - t0)

def wait_ready():
    while True:
        r = cmd(s, buf, "IDX.QUERY", "a_agg", "GROUP", "g0")
        if isinstance(r, list):
            return
        time.sleep(0.3)

_ = burst()  # warm
bases, taxeds = [], []
for _round in range(3):
    bases.append(burst())
    assert cmd(s, buf, "IDX.CREATE", "a_agg", "ON", "PREFIX", "a:", "FIELD", "val",
               "TYPE", "i64", "KIND", "agg", "GROUPBY", "grp") == b"+OK"
    wait_ready()
    _ = burst()  # settle
    taxeds.append(burst())
    cmd(s, buf, "IDX.DROP", "a_agg")
bases.sort(); taxeds.sort()
tax = (bases[1] - taxeds[1]) / bases[1] * 100
print(f"agggate: write tax bases={[int(b) for b in bases]} taxed={[int(t) for t in taxeds]} median tax={tax:.1f}%")
if tax >= 10.0:
    print(f"agggate: FAIL — write tax {tax:.1f}% >= 10%"); sys.exit(1)

# ---- rebuild for the query clamps + memory ----
rss_before = rss_kb()
assert cmd(s, buf, "IDX.CREATE", "a_agg", "ON", "PREFIX", "a:", "FIELD", "val",
           "TYPE", "i64", "KIND", "agg", "GROUPBY", "grp") == b"+OK"
t0 = time.time()
wait_ready()
print(f"agggate: built in {time.time()-t0:.1f}s", flush=True)

def conn_p99(query, per=200):
    out = []
    for _ in range(6):
        c = connect(); cb = [b""]
        lat = []
        for i in range(per):
            t = time.time()
            r = cmd(c, cb, *query(i))
            lat.append(time.time() - t)
            assert isinstance(r, list), r
        lat.sort()
        out.append(lat[int(per * 0.99) - 1] * 1000)
        c.close()
    out.sort()
    return out

p = conn_p99(lambda i: ("IDX.QUERY", "a_agg", "GROUP", f"g{i * 337 % GROUPS}"))
print(f"agggate: GROUP p99 median={p[3]:.3f}ms worst={p[5]:.3f}ms")
if p[3] >= 1.0:
    print(f"agggate: FAIL — GROUP p99 {p[3]:.3f}ms >= 1ms"); sys.exit(1)

p = conn_p99(lambda i: ("IDX.QUERY", "a_agg", "GROUPS", "BY", "sum", "LIMIT", "100"), per=100)
print(f"agggate: GROUPS top-100 p99 median={p[3]:.3f}ms worst={p[5]:.3f}ms")
if p[3] >= 5.0:
    print(f"agggate: FAIL — GROUPS p99 {p[3]:.3f}ms >= 5ms"); sys.exit(1)

r = cmd(s, buf, "IDX.VERIFY", "a_agg")
kv = {r[i].decode(): r[i+1].decode() for i in range(0, len(r), 2)}
formula = int(kv["bytes"])
growth = (rss_kb() - rss_before) * 1024
ratio = formula / growth if growth > 0 else 0
print(f"agggate: formula={formula/2**20:.0f}MiB rss-growth={growth/2**20:.0f}MiB ratio={ratio:.2f}")
if not (0.5 <= ratio <= 1.5):
    print(f"agggate: FAIL — formula explains {ratio:.2f}x of RSS growth"); sys.exit(1)
print("agggate: PASS")
PYEOF
