#!/bin/bash
# v2.11 serving-shape gate — the serving-engine arc's headline lines
# measured on ONE server carrying the full P2/P3 stack at once:
# 1M rows, two scalar indexes, one materialized top-K view.
#
#   1. ROW-LIST page: IDX.QUERY RANGE LIMIT 20 FIELDS (hydrated) p99
#      < 1ms, median of 6 connections.
#   2. VIEW page: VIEW.QUERY LIMIT 20 p99 < 1ms.
#   3. WRITE FAN-OUT: HSET touching 2 indexes + 1 materialized view,
#      p99 < 200µs (pipelined-1, per-op RTT).
#
# Usage: bash bench/servinggate.sh <kevy-binary>
set -u
BIN=${1:?usage: servinggate.sh <kevy-binary>}
PORT=7081
DIR=$(mktemp -d /tmp/kevy-servinggate-XXXXXX)

PIN=""
command -v taskset >/dev/null 2>&1 && PIN="taskset -c 0-7"
CLIENT_PIN=""
command -v taskset >/dev/null 2>&1 && CLIENT_PIN="taskset -c 8-15"
env KEVY_BIND=127.0.0.1 $PIN "$BIN" --threads 8 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SRV=$!
sleep 1.2
trap 'kill $SRV 2>/dev/null; rm -rf "$DIR"' EXIT

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
    raise RuntimeError(l)

def cmd(sock, buf, *parts):
    sock.sendall(enc(*parts))
    return read_reply(sock, buf)

N = 1_000_000
s = connect(); buf = [b""]
t0 = time.time()
batch = []
for i in range(N):
    batch.append(enc("HSET", f"r:{i}", "ts", str(i), "grp", str(i % 100), "body", f"row body {i}"))
    if len(batch) == 2000:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            read_reply(s, buf)
        batch = []
if batch:
    s.sendall(b"".join(batch))
    for _ in range(len(batch)):
        read_reply(s, buf)
print(f"servinggate: loaded {N} rows in {time.time()-t0:.1f}s", flush=True)

for spec in (("r_ts", "ts"), ("r_grp", "grp")):
    r = cmd(s, buf, "IDX.CREATE", spec[0], "ON", "PREFIX", "r:", "FIELD", spec[1], "TYPE", "i64", "KIND", "range")
    assert r == b"+OK", r
t0 = time.time()
while True:
    r = cmd(s, buf, "IDX.QUERY", "r_ts", "RANGE", "0", "1", "LIMIT", "1")
    if isinstance(r, list):
        break
    if time.time() - t0 > 300:
        print("servinggate: FAIL — index build timeout"); sys.exit(1)
    time.sleep(0.2)
r = cmd(s, buf, "VIEW.CREATE", "v_hot", "QUERY", "(", "AND",
        "r_ts", "RANGE", "0", "500000", "r_grp", "RANGE", "0", "50", ")",
        "ORDER", "BY", "r_ts", "DESC", "MODE", "materialized", "TOPK", "100")
assert r == b"+OK", r
while True:
    r = cmd(s, buf, "VIEW.QUERY", "v_hot", "LIMIT", "1")
    if isinstance(r, list):
        break
    time.sleep(0.2)
print("servinggate: stack ready (2 indexes + 1 materialized view)", flush=True)

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

# clamp 1: hydrated row-list page
p = conn_p99(lambda i: ("IDX.QUERY", "r_ts", "RANGE", str(i * 331 % 900000), str(i * 331 % 900000 + 5000), "LIMIT", "20", "FIELDS", "body"))
print(f"servinggate: row-list p99 median={p[3]:.3f}ms worst={p[5]:.3f}ms")
if p[3] >= 1.0:
    print(f"servinggate: FAIL — row-list p99 {p[3]:.3f}ms >= 1ms"); sys.exit(1)

# clamp 2: view page
p = conn_p99(lambda i: ("VIEW.QUERY", "v_hot", "LIMIT", "20"))
print(f"servinggate: view p99 median={p[3]:.3f}ms worst={p[5]:.3f}ms")
if p[3] >= 1.0:
    print(f"servinggate: FAIL — view p99 {p[3]:.3f}ms >= 1ms"); sys.exit(1)

# clamp 3: write fan-out p99 < 200µs (HSET → 2 index hooks + view hook)
p99s = []
for _ in range(6):
    c = connect(); cb = [b""]
    lat = []
    for i in range(500):
        k = random.randrange(0, N)
        t = time.time()
        r = cmd(c, cb, "HSET", f"r:{k}", "ts", str(random.randrange(0, N)))
        lat.append(time.time() - t)
        assert r[:1] == b":", r
    lat.sort()
    p99s.append(lat[494] * 1000)
    c.close()
p99s.sort()
print(f"servinggate: write fan-out p99 median={p99s[3]*1000:.0f}µs worst={p99s[5]*1000:.0f}µs")
if p99s[3] >= 0.2:
    print(f"servinggate: FAIL — write p99 {p99s[3]*1000:.0f}µs >= 200µs"); sys.exit(1)
print("servinggate: PASS")
PYEOF
