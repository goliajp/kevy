#!/usr/bin/env python3
"""v3.3 baseline arena — SERVING FACE: kevy's index engine vs
redis-stack (RediSearch) on four query classes over EQUIVALENT
corpora (the gates' corpus epistemology: Zipf text, manifold vectors,
Zipf groups):

  FTS      kevy IDX.QUERY MATCH        vs FT.SEARCH (BM25)
  ANN      kevy IDX.QUERY KNN          vs FT.SEARCH KNN (HNSW)
  AGG      kevy IDX.QUERY GROUPS       vs FT.AGGREGATE GROUPBY
  NUMERIC  kevy IDX.QUERY RANGE+FIELDS vs FT.SEARCH @val:[a b]

Protocol: both servers measured one at a time on the same cores by
the caller (arena.sh discipline); this script talks to whichever
port it's given. median-of-R query rounds + sample stdev per class.

usage: arena_serving.py (kevy|stack) <port> [--docs N] [--rounds R]
Emits: "class p50_ms p95_ms qps stdev_p95" rows.
"""

import random
import socket
import statistics
import struct
import sys
import time

MODE = sys.argv[1]
PORT = int(sys.argv[2])
DOCS = int(sys.argv[sys.argv.index("--docs") + 1]) if "--docs" in sys.argv else 200_000
ROUNDS = int(sys.argv[sys.argv.index("--rounds") + 1]) if "--rounds" in sys.argv else 5
DIM = 128
GROUPS = 10_000
QUERIES = 200


def connect():
    s = socket.create_connection(("127.0.0.1", PORT))
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
        out, buf[0] = buf[0][:n], buf[0][n + 2 :]
        return out
    if t == b"*":
        return [read_reply(sock, buf) for _ in range(int(body))]
    raise RuntimeError(l)


def cmd(sock, buf, *parts):
    sock.sendall(enc(*parts))
    r = read_reply(sock, buf)
    if isinstance(r, bytes) and r.startswith(b"-"):
        raise RuntimeError(r[:120])
    return r


def pipeline(sock, buf, frames):
    sock.sendall(b"".join(frames))
    return [read_reply(sock, buf) for _ in frames]


# ---------------- corpus (seeded — identical on both servers) -------
random.seed(11)
VOCAB = [f"w{r}" for r in range(10_000)]


def pick_word():
    u = random.random()
    return VOCAB[min(int(10_000**u) - 1, 9_999)]


def zipf_group():
    u = random.random()
    return min(int(GROUPS**u) - 1, GROUPS - 1)


LATENT = 20
W = [[random.gauss(0, 1) for _ in range(DIM)] for _ in range(LATENT)]


def manifold_vec():
    z = [random.uniform(-1, 1) for _ in range(LATENT)]
    v = [sum(z[k] * W[k][d] for k in range(LATENT)) + random.gauss(0, 0.05) for d in range(DIM)]
    return v


def blob(v):
    return struct.pack(f"<{DIM}f", *v)


def load(sock, buf):
    t0 = time.time()
    frames = []
    for i in range(DOCS):
        body = " ".join(pick_word() for _ in range(10))
        frames.append(
            enc(
                "HSET",
                f"a:{i}",
                "body",
                body,
                "grp",
                f"g{zipf_group()}",
                "val",
                str(random.randrange(1_000_000)),
                "v",
                blob(manifold_vec()),
            )
        )
        if len(frames) == 500:
            pipeline(sock, buf, frames)
            frames = []
    if frames:
        pipeline(sock, buf, frames)
    print(f"# loaded {DOCS} docs in {time.time()-t0:.1f}s", file=sys.stderr)


def declare_kevy(sock, buf):
    cmd(sock, buf, "IDX.CREATE", "ax_t", "ON", "PREFIX", "a:", "FIELD", "body", "TYPE", "str", "KIND", "text")
    cmd(sock, buf, "IDX.CREATE", "ax_v", "ON", "PREFIX", "a:", "FIELD", "v", "TYPE", "vector", "KIND", "ann", "DIM", str(DIM), "DISTANCE", "l2")
    cmd(sock, buf, "IDX.CREATE", "ax_g", "ON", "PREFIX", "a:", "FIELD", "val", "TYPE", "i64", "KIND", "agg", "GROUPBY", "grp")
    cmd(sock, buf, "IDX.CREATE", "ax_n", "ON", "PREFIX", "a:", "FIELD", "val", "TYPE", "i64", "KIND", "range")
    t0 = time.time()
    while True:
        ok = True
        for probe in (
            ("IDX.QUERY", "ax_t", "MATCH", "w0", "LIMIT", "1"),
            ("IDX.QUERY", "ax_v", "KNN", blob(manifold_vec()), "LIMIT", "1"),
            ("IDX.QUERY", "ax_g", "GROUP", "g0"),
            ("IDX.QUERY", "ax_n", "RANGE", "0", "10", "LIMIT", "1"),
        ):
            try:
                cmd(sock, buf, *probe)
            except RuntimeError:
                ok = False
                break
        if ok:
            break
        if time.time() - t0 > 900:
            raise SystemExit("kevy index build timeout")
        time.sleep(1)
    print(f"# kevy indexes ready in {time.time()-t0:.1f}s", file=sys.stderr)


def declare_stack(sock, buf):
    t0 = time.time()
    cmd(
        sock, buf,
        "FT.CREATE", "ax", "ON", "HASH", "PREFIX", "1", "a:",
        "SCHEMA",
        "body", "TEXT",
        "grp", "TAG",
        "val", "NUMERIC",
        "v", "VECTOR", "HNSW", "6", "TYPE", "FLOAT32", "DIM", str(DIM), "DISTANCE_METRIC", "L2",
    )
    while True:
        info = cmd(sock, buf, "FT.INFO", "ax")
        d = {info[i]: info[i + 1] for i in range(0, len(info) - 1, 2) if isinstance(info[i], bytes)}
        # ints come back as b":0" through this thin reader
        v = d.get(b"indexing", b":1")
        if isinstance(v, bytes) and v.lstrip(b":") == b"0":
            break
        if time.time() - t0 > 900:
            raise SystemExit("stack index build timeout")
        time.sleep(1)
    print(f"# stack index ready in {time.time()-t0:.1f}s", file=sys.stderr)


def measure(sock, buf, label, make_query):
    p95s, p50s, qpss = [], [], []
    for _ in range(ROUNDS):
        lat = []
        t0 = time.time()
        for i in range(QUERIES):
            q = make_query(i)
            t = time.time()
            cmd(sock, buf, *q)
            lat.append(time.time() - t)
        dt = time.time() - t0
        lat.sort()
        p50s.append(lat[QUERIES // 2] * 1000)
        p95s.append(lat[int(QUERIES * 0.95)] * 1000)
        qpss.append(QUERIES / dt)
    p95s.sort(); p50s.sort(); qpss.sort()
    sd = statistics.stdev(p95s) if len(p95s) > 1 else 0.0
    print(f"{label} {p50s[ROUNDS//2]:.3f} {p95s[ROUNDS//2]:.3f} {qpss[ROUNDS//2]:.0f} {sd:.3f}")


def main():
    sock = connect()
    buf = [b""]
    load(sock, buf)
    if MODE == "kevy":
        declare_kevy(sock, buf)
        fts = lambda i: ("IDX.QUERY", "ax_t", "MATCH", f"w{i % 7} w{(i * 331) % 9000 + 500}", "LIMIT", "10")
        ann = lambda i: ("IDX.QUERY", "ax_v", "KNN", blob(manifold_vec()), "LIMIT", "10", "EF", "400")
        agg = lambda i: ("IDX.QUERY", "ax_g", "GROUPS", "BY", "sum", "LIMIT", "100")
        num = lambda i: ("IDX.QUERY", "ax_n", "RANGE", str(i * 4337 % 900_000), str(i * 4337 % 900_000 + 5000), "LIMIT", "20", "FIELDS", "body")
    else:
        declare_stack(sock, buf)
        fts = lambda i: ("FT.SEARCH", "ax", f"w{i % 7} | w{(i * 331) % 9000 + 500}", "LIMIT", "0", "10")
        ann = lambda i: ("FT.SEARCH", "ax", "*=>[KNN 10 @v $B EF_RUNTIME 400]", "PARAMS", "2", "B", blob(manifold_vec()), "DIALECT", "2", "LIMIT", "0", "10")
        agg = lambda i: ("FT.AGGREGATE", "ax", "*", "GROUPBY", "1", "@grp", "REDUCE", "SUM", "1", "@val", "AS", "s", "SORTBY", "2", "@s", "DESC", "LIMIT", "0", "100")
        num = lambda i: ("FT.SEARCH", "ax", f"@val:[{i * 4337 % 900_000} {i * 4337 % 900_000 + 5000}]", "RETURN", "1", "body", "LIMIT", "0", "20")
    print("class p50_ms p95_ms qps stdev_p95")
    measure(sock, buf, "FTS", fts)
    measure(sock, buf, "ANN", ann)
    measure(sock, buf, "AGG", agg)
    measure(sock, buf, "NUMERIC", num)


main()
