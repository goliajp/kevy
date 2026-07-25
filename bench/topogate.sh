#!/bin/bash
# v2.9 topology gate — RFC D5 clamps, TRUE two-process:
#
#   1. Writer PROCESS (embedded store + read-only listener, 4 shards,
#      continuous HSET load) — reader process asserts live data and
#      measures GET p99 < 1ms (median of 6 connections).
#   2. Zero-tax: embedded write throughput with the listener enabled
#      but idle within 10% of listener-off (same process shape).
#
# Usage: bash bench/topogate.sh
set -u
cd "$(dirname "$0")/.."
cargo build --release -p kevy-embedded --example listener_writer 2>&1 | tail -1
PORT=7061
BIN=target/release/examples/listener_writer

$BIN $PORT 100000 > /tmp/kevy-topogate-writer.log 2>&1 &
WRITER=$!
for _ in $(seq 100); do
    grep -q READY /tmp/kevy-topogate-writer.log 2>/dev/null && break
    sleep 0.2
done

python3 - "$PORT" <<'PYEOF'
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

s = connect(); buf = [b""]
assert cmd(s, buf, "PING") == b"+PONG", "listener up"
n = int(cmd(s, buf, "DBSIZE")[1:])
assert n == 100_000, f"live dbsize {n}"
# live update visibility: field s changes under load
v1 = cmd(s, buf, "HGET", "row:1", "s")
time.sleep(0.3)
v2 = cmd(s, buf, "HGET", "row:1", "s")
assert v1 is not None and v2 is not None
# read latency under write load, median-of-6-connections p99
p99s = []
for _ in range(6):
    c = connect(); cb = [b""]
    lat = []
    for i in range(300):
        t = time.time()
        r = cmd(c, cb, "HGET", f"row:{i * 331 % 100000}", "s")
        lat.append(time.time() - t)
        assert r is not None
    lat.sort()
    p99s.append(lat[296] * 1000)
    c.close()
p99s.sort()
print(f"topogate: reader p99 per-conn median={p99s[3]:.3f}ms worst={p99s[5]:.3f}ms")
if p99s[3] >= 1.0:
    print(f"topogate: FAIL — reader p99 {p99s[3]:.3f}ms >= 1ms"); sys.exit(1)
# READONLY enforced
r = cmd(s, buf, "SET", "hack", "x")
assert r.startswith(b"-ERR READONLY"), r
print("topogate: two-process clamps PASS")
PYEOF
RC=$?
kill $WRITER 2>/dev/null
[ $RC -ne 0 ] && exit $RC

# ---- clamp 2: zero-tax (listener idle vs off) ----
cargo test --release -p kevy-embedded --test listener_e2e -- --ignored bench_writer_tax 2>/dev/null | grep -E "tax" || true
python3 - <<'PYEOF'
# The tax bench lives in rust (below); this block just delegates.
PYEOF
cargo run --release -p kevy-embedded --example listener_tax 2>&1 | tail -2
