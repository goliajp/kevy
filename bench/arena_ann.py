#!/usr/bin/env python3
"""v3.6 Phase A0 — recall-latency pareto alignment, kevy KNN vs
RediSearch HNSW. The v3.3 arena compared EF 400 vs EF_RUNTIME 400
NOMINALLY — same knob value is not the same recall. This harness:

  1. loads the same seeded manifold corpus into both engines,
  2. uses a redis-stack FLAT index as the EXACT ground-truth oracle,
  3. sweeps the search-width knob on both engines,
  4. prints (engine, knob, recall@10, p50 ms, p95 ms) rows —
     the gap verdict is read AT EQUAL RECALL, not equal knob.

usage: arena_ann.py (kevy|stack|truth) <port> [--vecs N]
`truth` mode expects a redis-stack port; it builds the FLAT oracle
and prints the ground-truth ids per query to stdout (cached to a
file by the caller). kevy/stack modes read that file on stdin.
"""

import random
import socket
import struct
import sys
import time

MODE = sys.argv[1]
PORT = int(sys.argv[2])
VECS = int(sys.argv[sys.argv.index("--vecs") + 1]) if "--vecs" in sys.argv else 100_000
DIM = 128
QUERIES = 100
K = 10


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
        raise RuntimeError(r[:150])
    return r


def pipeline(sock, buf, frames):
    sock.sendall(b"".join(frames))
    return [read_reply(sock, buf) for _ in frames]


# ---- corpus: same seed everywhere ----
random.seed(23)
LATENT = 20
W = [[random.gauss(0, 1) for _ in range(DIM)] for _ in range(LATENT)]


def manifold_vec():
    z = [random.uniform(-1, 1) for _ in range(LATENT)]
    return [
        sum(z[k] * W[k][d] for k in range(LATENT)) + random.gauss(0, 0.05)
        for d in range(DIM)
    ]


def blob(v):
    return struct.pack(f"<{DIM}f", *v)


def load(sock, buf):
    t0 = time.time()
    frames = []
    for i in range(VECS):
        frames.append(enc("HSET", f"v:{i}", "v", blob(manifold_vec())))
        if len(frames) == 500:
            pipeline(sock, buf, frames)
            frames = []
    if frames:
        pipeline(sock, buf, frames)
    print(f"# loaded {VECS} vecs in {time.time()-t0:.1f}s", file=sys.stderr)


def query_vecs():
    # AFTER the corpus stream so the RNG state is deterministic for
    # every mode.
    return [manifold_vec() for _ in range(QUERIES)]


def ids_from_stack_reply(r):
    # FT.SEARCH reply: [count, key1, fields1, key2, fields2, ...]
    # with NOCONTENT: [count, key1, key2, ...]
    return [x.decode() for x in r[1:] if isinstance(x, bytes)]


def ids_from_kevy_reply(r):
    # IDX.QUERY KNN reply: flat array of keys (bulk strings).
    return [x.decode() for x in r if isinstance(x, bytes)]


def main():
    sock = connect()
    buf = [b""]
    load(sock, buf)
    qs = query_vecs()

    if MODE == "truth":
        cmd(
            sock, buf,
            "FT.CREATE", "vt", "ON", "HASH", "PREFIX", "1", "v:",
            "SCHEMA", "v", "VECTOR", "FLAT", "6",
            "TYPE", "FLOAT32", "DIM", str(DIM), "DISTANCE_METRIC", "L2",
        )
        t0 = time.time()
        while True:
            info = cmd(sock, buf, "FT.INFO", "vt")
            d = {
                info[i].lstrip(b"+"): info[i + 1]
                for i in range(0, len(info) - 1, 2)
                if isinstance(info[i], bytes)
            }
            v = d.get(b"indexing", b":1")
            if isinstance(v, bytes) and v.lstrip(b":+") == b"0":
                break
            if time.time() - t0 > 600:
                raise SystemExit("truth index build timeout")
            time.sleep(1)
        print(f"# FLAT oracle ready in {time.time()-t0:.1f}s", file=sys.stderr)
        for q in qs:
            r = cmd(
                sock, buf,
                "FT.SEARCH", "vt", f"*=>[KNN {K} @v $B]",
                "PARAMS", "2", "B", blob(q),
                "NOCONTENT", "DIALECT", "2", "LIMIT", "0", str(K),
            )
            print(" ".join(ids_from_stack_reply(r)))
        return

    truth = [set(line.split()) for line in sys.stdin.read().strip().split("\n")]
    assert len(truth) == QUERIES, f"want {QUERIES} truth rows, got {len(truth)}"

    if MODE == "kevy":
        cmd(sock, buf, "IDX.CREATE", "vx", "ON", "PREFIX", "v:", "FIELD", "v",
            "TYPE", "vector", "KIND", "ann", "DIM", str(DIM), "DISTANCE", "l2")
        t0 = time.time()
        while True:
            try:
                if isinstance(cmd(sock, buf, "IDX.QUERY", "vx", "KNN", blob(qs[0]), "LIMIT", "1"), list):
                    break
            except RuntimeError:
                pass
            if time.time() - t0 > 900:
                raise SystemExit("kevy index build timeout")
            time.sleep(1)
        print(f"# kevy ann ready in {time.time()-t0:.1f}s", file=sys.stderr)
        run = lambda q, knob: ids_from_kevy_reply(
            cmd(sock, buf, "IDX.QUERY", "vx", "KNN", blob(q), "LIMIT", str(K), "EF", str(knob))
        )
    else:
        cmd(
            sock, buf,
            "FT.CREATE", "vx", "ON", "HASH", "PREFIX", "1", "v:",
            "SCHEMA", "v", "VECTOR", "HNSW", "6",
            "TYPE", "FLOAT32", "DIM", str(DIM), "DISTANCE_METRIC", "L2",
        )
        t0 = time.time()
        while True:
            info = cmd(sock, buf, "FT.INFO", "vx")
            d = {
                info[i].lstrip(b"+"): info[i + 1]
                for i in range(0, len(info) - 1, 2)
                if isinstance(info[i], bytes)
            }
            v = d.get(b"indexing", b":1")
            if isinstance(v, bytes) and v.lstrip(b":+") == b"0":
                break
            if time.time() - t0 > 900:
                raise SystemExit("stack index build timeout")
            time.sleep(1)
        print(f"# stack hnsw ready in {time.time()-t0:.1f}s", file=sys.stderr)
        run = lambda q, knob: ids_from_stack_reply(
            cmd(
                sock, buf,
                "FT.SEARCH", "vx", f"*=>[KNN {K} @v $B EF_RUNTIME {knob}]",
                "PARAMS", "2", "B", blob(q),
                "NOCONTENT", "DIALECT", "2", "LIMIT", "0", str(K),
            )
        )

    print("engine knob recall p50_ms p95_ms")
    # kevy's EF knob floors at 16
    for knob in (16, 20, 50, 100, 200, 400, 800):
        hits = 0
        lat = []
        for rep in range(3):
            for qi, q in enumerate(qs):
                t = time.time()
                ids = run(q, knob)
                lat.append(time.time() - t)
                if rep == 0:
                    hits += len(set(ids) & truth[qi])
        lat.sort()
        recall = hits / (QUERIES * K)
        n = len(lat)
        print(
            f"{MODE} {knob} {recall:.3f} {lat[n//2]*1000:.3f} {lat[int(n*0.95)]*1000:.3f}"
        )


main()
