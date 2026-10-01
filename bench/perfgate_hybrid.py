#!/usr/bin/env python3
"""Hybrid-retrieval latency angle for perfgate.

  perfgate_hybrid.py load PORT          # docs + text/ann indexes, waits until ready
  perfgate_hybrid.py run PORT N         # p95 of IDX.QUERY HYBRID in whole µs

Timed per request in this client, one connection, closed loop. The caller
pins the client; redis-benchmark is not used because its p95 comes out on a
coarse grid (231/239/255/263 µs on this query), wider than the gate's band.
"""
import random
import socket
import struct
import sys
import time

DOCS, DIM, LAT, VOCAB = 20000, 32, 8, 500
WARM = 1000


def resp(*args):
    out = [b"*%d\r\n" % len(args)]
    for a in args:
        a = a if isinstance(a, bytes) else str(a).encode()
        out.append(b"$%d\r\n%s\r\n" % (len(a), a))
    return b"".join(out)


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port))
        self.f = self.s.makefile("rb")

    def send(self, payload):
        self.s.sendall(payload)

    def reply(self):
        line = self.f.readline()
        kind, rest = line[:1], line[1:-2]
        if kind in (b"+", b"-", b":"):
            return line[:-2]
        if kind == b"$":
            n = int(rest)
            return None if n < 0 else self.f.read(n + 2)[:-2]
        if kind == b"*":
            n = int(rest)
            return None if n < 0 else [self.reply() for _ in range(n)]
        raise SystemExit(f"perfgate_hybrid: unexpected reply {line!r}")

    def cmd(self, *args):
        self.send(resp(*args))
        return self.reply()


def model(rng):
    w = [[rng.gauss(0, 1) for _ in range(DIM)] for _ in range(LAT)]

    def vec():
        z = [rng.uniform(-1, 1) for _ in range(LAT)]
        return [sum(z[l] * w[l][d] for l in range(LAT)) for d in range(DIM)]

    return vec


def word(rng):
    # zipf-ish: a few words are common, most are rare
    return f"h{min(int(VOCAB ** rng.random()) - 1, VOCAB - 1)}"


def ready(c, qvec):
    for args in (("hb_t", "MATCH", "h0"), ("hb_v", "KNN", qvec)):
        r = c.cmd("IDX.EXPLAIN", *args, "LIMIT", "10")
        pairs = {p[0]: p[1] for p in r if isinstance(p, list) and len(p) == 2} if isinstance(r, list) else {}
        if pairs.get(b"state") != b"ready":
            return False
    return True


def load(port):
    rng = random.Random(31)
    vec = model(rng)
    c = Conn(port)
    batch = []
    for i in range(DOCS):
        body = " ".join(word(rng) for _ in range(8))
        batch.append(resp("HSET", f"hb:{i}", "body", body, "v", struct.pack(f"<{DIM}f", *vec())))
        if len(batch) == 500 or i == DOCS - 1:
            c.send(b"".join(batch))
            for _ in batch:
                c.reply()
            batch = []
    c.cmd("IDX.CREATE", "hb_t", "ON", "PREFIX", "hb:", "FIELD", "body", "TYPE", "str", "KIND", "text")
    c.cmd("IDX.CREATE", "hb_v", "ON", "PREFIX", "hb:", "FIELD", "v", "TYPE", "vector",
          "KIND", "ann", "DIM", str(DIM), "DISTANCE", "l2")
    qvec = struct.pack(f"<{DIM}f", *vec())
    deadline = time.time() + 120
    while not ready(c, qvec):
        if time.time() > deadline:
            raise SystemExit("perfgate_hybrid: indexes not ready after 120s")
        time.sleep(0.2)


def run(port, n):
    qvec = struct.pack(f"<{DIM}f", *model(random.Random(31))())
    c = Conn(port)
    req = resp("IDX.QUERY", "HYBRID", "hb_t", "MATCH", "h7", "hb_v", "KNN", qvec, "LIMIT", "10")
    probe = c.cmd("IDX.QUERY", "HYBRID", "hb_t", "MATCH", "h7", "hb_v", "KNN", qvec, "LIMIT", "10")
    if not (isinstance(probe, list) and len(probe) == 10):
        raise SystemExit(f"perfgate_hybrid: probe query did not return 10 rows: {probe!r}"[:200])
    lat = []
    for i in range(WARM + n):
        t = time.perf_counter_ns()
        c.send(req)
        c.reply()
        if i >= WARM:
            lat.append(time.perf_counter_ns() - t)
    lat.sort()
    print(round(lat[int(len(lat) * 0.95)] / 1000))


if __name__ == "__main__":
    if sys.argv[1] == "load":
        load(int(sys.argv[2]))
    else:
        run(int(sys.argv[2]), int(sys.argv[3]))
