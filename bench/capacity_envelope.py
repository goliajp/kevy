#!/usr/bin/env python3
"""capacity_envelope.py — the RESP driver behind bench/capacity-envelope.sh.

Pure stdlib (the house 0-dep discipline extends to bench drivers): a
minimal RESP2 client + the envelope's load / latency / sweep bodies.
The shell script owns server lifecycle, guards, RSS sampling and the
verdicts; this file owns everything that talks RESP.

Subcommands:
  load-d1  --port P --rows N --pad B --seed S       pipelined HSET rows (row:<i>)
  load-b6  --port P --keys N --val B --seed S [--start I]  pipelined SET (b6:<i>)
  lat      --port P --n N --seed S --rows R --mode M [--fields]
           modes: c4 c5 where hydrate hot coldget coldrow digest
           prints: n=.. errors=.. p50_us=.. p95_us=.. p99_us=..
  sweep    --port P --keys N                        the B6 op sweep (exit != 0 on any wrong shape)
  info     --port P --field F                       one INFO tiering gauge
  cmd      --port P -- VERB ARG...                  one command, compact-rendered reply
"""

import random
import socket
import sys
import time


# ---------- RESP2 ----------

def enc(args):
    out = [b"*%d\r\n" % len(args)]
    for a in args:
        if isinstance(a, str):
            a = a.encode()
        out.append(b"$%d\r\n%s\r\n" % (len(a), a))
    return b"".join(out)


def read_reply(f):
    line = f.readline()
    if not line:
        raise ConnectionError("server closed the connection")
    t, body = line[:1], line[1:-2]
    if t == b"+":
        return ("ok", body)
    if t == b"-":
        return ("err", body)
    if t == b":":
        return ("int", int(body))
    if t == b"$":
        n = int(body)
        if n < 0:
            return ("nil", None)
        data = f.read(n + 2)[:-2]
        return ("bulk", data)
    if t == b"*":
        n = int(body)
        if n < 0:
            return ("nil", None)
        return ("arr", [read_reply(f) for _ in range(n)])
    raise ValueError("bad RESP type byte %r" % t)


class Client:
    def __init__(self, port):
        # 600s: a full-scale bulk-load reply can lag behind sustained
        # spill/compaction; 60s produced a false driver timeout at B6.
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=600)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.f = self.sock.makefile("rb")

    def req(self, args):
        self.sock.sendall(enc(args))
        return read_reply(self.f)

    def pipeline(self, frames):
        """Send `frames` (already-encoded commands) as one write, read
        that many replies; returns the error replies."""
        self.sock.sendall(b"".join(frames))
        errs = []
        for _ in range(len(frames)):
            r = read_reply(self.f)
            if r[0] == "err":
                errs.append(r[1])
        return errs


def render(r):
    t, v = r
    if t == "ok":
        return "+" + v.decode(errors="replace")
    if t == "err":
        return "-" + v.decode(errors="replace")
    if t == "int":
        return ":%d" % v
    if t == "bulk":
        return v.decode(errors="replace")
    if t == "nil":
        return "(nil)"
    return "*%d" % len(v)


def opt(name, default=None):
    if name in sys.argv:
        return sys.argv[sys.argv.index(name) + 1]
    if default is None:
        sys.exit("capacity_envelope.py: missing %s" % name)
    return default


# ---------- deterministic row derivation (the seeded loader) ----------
# Row i: status s<i%5>, score = (i*2654435761) % (rows*8) (a fixed odd
# multiplier — Knuth's), ts = i, pad = one seeded blob. The lat modes
# re-derive the same values client-side, so a query can target a value
# that exists without asking the server first.

SCORE_MUL = 2654435761


def score_of(i, rows):
    return (i * SCORE_MUL) % (rows * 8)


def run_load_d1():
    port, rows = int(opt("--port")), int(opt("--rows"))
    pad_len, seed = int(opt("--pad", "900")), int(opt("--seed", "1"))
    pad = bytes(random.Random(seed).randrange(33, 127) for _ in range(pad_len))
    c = Client(port)
    batch, t0 = [], time.time()
    for i in range(rows):
        batch.append(enc([
            b"HSET", b"row:%d" % i, b"id", b"%d" % i,
            b"status", b"s%d" % (i % 5), b"score", b"%d" % score_of(i, rows),
            b"ts", b"%d" % i, b"pad", pad,
        ]))
        if len(batch) == 512:
            errs = c.pipeline(batch)
            if errs:
                sys.exit("load-d1: server error: %s" % errs[0].decode())
            batch = []
        if i and i % 1_000_000 == 0:
            print("load-d1: %d rows (%.0fs)" % (i, time.time() - t0), file=sys.stderr)
    if batch:
        errs = c.pipeline(batch)
        if errs:
            sys.exit("load-d1: server error: %s" % errs[0].decode())
    print("load-d1: %d rows in %.1fs" % (rows, time.time() - t0))


def run_load_b6():
    port, keys = int(opt("--port")), int(opt("--keys"))
    val_len, seed = int(opt("--val", "4096")), int(opt("--seed", "1"))
    start = int(opt("--start", "0"))
    val = bytes(random.Random(seed).randrange(0, 256) for _ in range(val_len))
    c = Client(port)
    # Bound the pipeline by IN-FLIGHT BYTES, not a fixed frame count. A
    # blind 512-deep pipeline of 4KiB values is a ~2MB single send that
    # trips a reactor flow-control wedge (the server backpressures its
    # recv while the client is mid-send, and neither side progresses —
    # reproducible on a PLAIN non-tiered server at ~16MB, orthogonal to
    # tiering; see PERF-FINDING-2026-07-25-uring-deep-pipeline-wedge.md).
    # ~128KB in flight is a realistic bulk-load client and measures the
    # capacity behaviour B6 is actually about (demotion bounding RSS),
    # not that unrelated reactor bug.
    depth = max(1, 131072 // (val_len + 40))
    batch, t0 = [], time.time()
    for i in range(start, start + keys):
        batch.append(enc([b"SET", b"b6:%d" % i, val]))
        if len(batch) == depth:
            errs = c.pipeline(batch)
            if errs:
                sys.exit("load-b6: server error: %s" % errs[0].decode())
            batch = []
        if i and i % 1_000_000 == 0:
            print("load-b6: %d keys (%.0fs)" % (i, time.time() - t0), file=sys.stderr)
    if batch:
        errs = c.pipeline(batch)
        if errs:
            sys.exit("load-b6: server error: %s" % errs[0].decode())
    print("load-b6: %d keys in %.1fs" % (keys, time.time() - t0))


# ---------- latency sampling ----------

def build_cmd(mode, rng, rows, fields):
    """One sample command for `mode` (see the module docstring)."""
    if mode == "c4":  # indexed point lookup: EQ a score that exists
        i = rng.randrange(rows)
        return [b"IDX.QUERY", b"env.score", b"EQ", b"%d" % score_of(i, rows), b"LIMIT", b"20"]
    if mode == "c5":  # driving range + residual FILTER + SORT + page
        span = 16000  # score domain is rows*8 -> ~2000 candidate rows
        a = rng.randrange(max(rows * 8 - span, 1))
        return [b"IDX.QUERY", b"env.score", b"RANGE", b"%d" % a, b"%d" % (a + span),
                b"FILTER", b"status", b"EQ", b"s3", b"SORT", b"ts", b"DESC", b"LIMIT", b"20"]
    if mode in ("where", "hydrate"):  # composite page; hydrate adds FIELDS
        # ts window inside the first 30% of the keyspace = the region
        # that demoted first (LRU) — ~800 candidate rows per window.
        t0 = rng.randrange(max(int(rows * 0.3) - 4000, 1))
        cmd = [b"IDX.QUERY", b"env.by_status_ts", b"WHERE", b"status", b"EQ",
               b"s%d" % rng.randrange(5), b"RANGE", b"ts", b"%d" % t0,
               b"%d" % (t0 + 4000), b"LIMIT", b"20"]
        if mode == "hydrate":
            cmd += [b"FIELDS"] + [f.encode() for f in fields]
        return cmd
    if mode == "hot":
        # The hot working set = the last 1000 rows, deliberately narrow
        # so a warmup pass revisits keys (promotion is on the SECOND
        # access) and the measured set is genuinely resident.
        lo = max(rows - 1000, 0)
        return [b"HGET", b"row:%d" % rng.randrange(lo, rows), b"id"]
    if mode == "coldrow":  # whole-row materialization on a cold hash
        return [b"HGETALL", b"row:%d" % rng.randrange(int(rows * 0.3))]
    if mode == "coldget":  # scalar cold point read
        return [b"GET", b"b6:%d" % rng.randrange(int(rows * 0.3))]
    sys.exit("lat: unknown mode %s" % mode)


def run_lat():
    port, n = int(opt("--port")), int(opt("--n"))
    rows, seed = int(opt("--rows")), int(opt("--seed", "7"))
    mode = opt("--mode")
    fields = opt("--fields", "status,ts,pad").split(",")
    rng = random.Random(seed)
    c = Client(port)
    if mode == "digest":  # one long bulk cold sweep (no percentiles)
        c.sock.settimeout(600)  # a full-tier sweep outlives the 60s default
        t0 = time.time()
        r = c.req([b"PREFIX.DIGEST", b"row:"])
        print("digest: %s in %.1fs" % (render(r)[:32], time.time() - t0))
        return
    # cold* modes must sample DISTINCT keys: the promotion gate installs
    # a value on its SECOND access, so re-sampling a key would measure a
    # promoted (hot) read and call it cold.
    picks = None
    if mode in ("coldrow", "coldget"):
        picks = iter(rng.sample(range(int(rows * 0.3)), n))
    lats, errors = [], 0
    for _ in range(n):
        if picks is not None:
            i = next(picks)
            key = b"row:%d" % i if mode == "coldrow" else b"b6:%d" % i
            cmd = [b"HGETALL", key] if mode == "coldrow" else [b"GET", key]
        else:
            cmd = build_cmd(mode, rng, rows, fields)
        t0 = time.perf_counter_ns()
        r = c.req(cmd)
        lats.append(time.perf_counter_ns() - t0)
        if r[0] == "err":
            errors += 1
    lats.sort()
    pct = lambda p: lats[min(int(len(lats) * p), len(lats) - 1)] // 1000
    print("mode=%s n=%d errors=%d p50_us=%d p95_us=%d p99_us=%d"
          % (mode, n, errors, pct(0.50), pct(0.95), pct(0.99)))
    if errors:
        sys.exit(1)


# ---------- the B6 op sweep ----------

def run_sweep():
    port, keys = int(opt("--port")), int(opt("--keys"))
    c = Client(port)
    checks = []

    def ck(name, got, want):
        checks.append((name, got == want, got, want))

    ck("EXISTS cold", c.req([b"EXISTS", b"b6:1"]), ("int", 1))
    ck("TYPE cold (no disk read)", c.req([b"TYPE", b"b6:1"]), ("ok", b"string"))
    ck("TTL cold", c.req([b"TTL", b"b6:1"]), ("int", -1))
    ck("EXPIRE cold", c.req([b"EXPIRE", b"b6:1", b"1000"]), ("int", 1))
    ck("PERSIST cold", c.req([b"PERSIST", b"b6:1"]), ("int", 1))
    ck("RENAME cold", c.req([b"RENAME", b"b6:1", b"b6:sweep"]), ("ok", b"OK"))
    ck("RENAME back", c.req([b"RENAME", b"b6:sweep", b"b6:1"]), ("ok", b"OK"))
    t, v = c.req([b"GET", b"b6:2"])
    ck("GET cold length", (t, len(v) if t == "bulk" else v), ("bulk", 4096))
    ck("DEL cold", c.req([b"DEL", b"b6:3"]), ("int", 1))
    ck("EXISTS deleted", c.req([b"EXISTS", b"b6:3"]), ("int", 0))
    ck("SET NX over deleted", c.req([b"SET", b"b6:3", b"x", b"NX"]), ("ok", b"OK"))
    ck("SET NX over cold", c.req([b"SET", b"b6:4", b"x", b"NX"]), ("nil", None))
    t, v = c.req([b"SCAN", b"0", b"COUNT", b"100"])
    ck("SCAN shape", (t, len(v) if t == "arr" else v), ("arr", 2))
    t, v = c.req([b"DBSIZE"])
    ck("DBSIZE >= keys", (t, v >= keys if t == "int" else v), ("int", True))
    bad = [c for c in checks if not c[1]]
    for name, okc, got, want in checks:
        print("  sweep %-24s %s" % (name, "ok" if okc else "FAIL got=%r want=%r" % (got, want)))
    if bad:
        sys.exit("sweep: %d/%d checks failed" % (len(bad), len(checks)))
    print("sweep: %d/%d ok" % (len(checks), len(checks)))


# ---------- INFO / one-shot ----------

def run_info():
    c = Client(int(opt("--port")))
    field = opt("--field")
    t, v = c.req([b"INFO", b"tiering"])
    if t != "bulk":
        sys.exit("info: unexpected reply %s" % t)
    for line in v.decode().splitlines():
        if line.startswith(field + ":"):
            print(line.split(":", 1)[1].strip())
            return
    print("")  # absent (tiering off) — caller decides


def run_cmd():
    c = Client(int(opt("--port")))
    args = sys.argv[sys.argv.index("--") + 1:]
    print(render(c.req([a.encode() for a in args])))


def main():
    sub = sys.argv[1] if len(sys.argv) > 1 else ""
    fn = {"load-d1": run_load_d1, "load-b6": run_load_b6, "lat": run_lat,
          "sweep": run_sweep, "info": run_info, "cmd": run_cmd}.get(sub)
    if fn is None:
        sys.exit(__doc__)
    fn()


if __name__ == "__main__":
    main()
