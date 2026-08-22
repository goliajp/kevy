#!/usr/bin/env python3
"""kevy vs PostgreSQL 18 — one harness, both systems, same timing code.

WHY ONE HARNESS. Timing two databases with their own native tools
(pgbench vs redis-benchmark) compares the tools as much as the engines.
Here a single Python process drives both, so the client-side cost —
socket syscalls, the interpreter loop — lands on both sides. That cost
is *additive*, so it compresses whatever ratio exists: every advantage
this reports is a LOWER bound, never an inflated one.

FAIRNESS, STATED UP FRONT. PostgreSQL 18 runs its stock configuration,
as asked: shared_buffers=128MB, fsync=on, synchronous_commit=on,
full_page_writes=on. kevy's stock configuration has AOF *off* — no
durability at all — so "default vs default" would be a category error,
not a benchmark. kevy is therefore measured in three durability modes
and every one is reported:

    none      AOF off (kevy's literal default; the in-memory ceiling)
    everysec  AOF on, background fsync each second (its persistent default)
    always    AOF on, fsync per write (the closest match to PG's
              synchronous_commit=on)

Read the row whose durability matches what you would actually run.

WHAT IS MEASURED. One table, the single-table shape kevy claims — the
same rows, the same declared access paths, loaded from the same CSV.

    load        bulk ingest of the CSV (PG: COPY; kevy: pipelined writes)
    pk          point lookup by primary key
    idx         point lookup by a secondary indexed column
    page        filter + range + ORDER BY + LIMIT 20 (the list-page shape)
    write       single-row update latency
    disk        bytes on disk after the load settles
    rss         resident memory of the server process

Every size is normalised per MB of source CSV, so the comparison is
"what does this engine cost me per unit of data", not "which number is
bigger".

  bench/pgcompare.py gen    --rows N --out FILE
  bench/pgcompare.py pg     --csv FILE --dsn DSN [--samples N]
  bench/pgcompare.py kevy   --csv FILE --port P [--samples N]
"""
import csv
import json
import os
import random
import socket
import statistics
import subprocess
import sys
import time

DEPTS = ["eng", "ops", "sales", "hr", "legal", "design", "data", "support"]


def opt(name, default=None):
    if name in sys.argv:
        return sys.argv[sys.argv.index(name) + 1]
    if default is None:
        sys.exit(f"missing {name}")
    return default


# ── data ────────────────────────────────────────────────────────────────────

def run_gen():
    rows, out = int(opt("--rows")), opt("--out")
    pad_len = int(opt("--pad", "400"))
    rng = random.Random(42)
    # The pad MUST be incompressible. A constant pad ("x" * n) past
    # Postgres's ~2 KB TOAST threshold gets compressed roughly 25:1, which
    # silently turns a 12 GB dataset into 488 MB on PG's disk — it then
    # fits entirely in the cache the run was trying to overflow, and the
    # whole "data exceeds RAM" premise collapses. Measured that way once;
    # the numbers were meaningless. Random hex per row keeps both engines
    # storing what they were given. (That PG *can* compress a repetitive
    # payload and kevy cannot is a real difference — but it belongs in the
    # findings as its own line, not hidden inside every other column.)
    alphabet = "0123456789abcdef"
    # `sku` exists so the indexed-lookup shape actually scatters. With
    # `age` (60 values) or `dept` (8) a LIMIT 20 returns the same handful
    # of rows every time and their pages never leave cache, however large
    # the table — which is what made the first "data exceeds RAM" round
    # unable to test what it claimed. ~20 rows per sku, placed at random.
    skus = max(1, rows // 20)
    t0 = time.time()
    with open(out, "w", newline="") as f:
        w = csv.writer(f)
        for i in range(rows):
            pad = "".join(rng.choices(alphabet, k=pad_len))
            w.writerow([
                i,
                f"user{i:08d}",
                DEPTS[i % len(DEPTS)],
                18 + (i % 60),
                1700000000 + i,
                rng.randrange(skus),
                pad,
            ])
    size = os.path.getsize(out)
    print(json.dumps({
        "rows": rows, "bytes": size, "mb": round(size / 1048576, 1),
        "seconds": round(time.time() - t0, 1),
    }))


def csv_mb(path):
    return os.path.getsize(path) / 1048576


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p))]


def server_pids(marker):
    """PIDs whose cmdline carries `marker`, excluding us and our ancestors.

    Not `pgrep`: this process's own argv carries the data directory, and so
    does every shell above it, so a name match finds the measurer as readily
    as the measured. And not the binary's name either — `KEVY_BIN` points at
    `kevy-off` / `kevy-on` for the allocator comparison, and a pattern of
    "kevy --port N" matches neither, which is how a whole run reported a
    resident set of zero and a leaked server kept answering on the same port.
    """
    mine, p = set(), os.getpid()
    while p > 1:
        mine.add(p)
        try:
            p = int(open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            break
    found, parent = [], {}
    for d in os.listdir("/proc"):
        if not d.isdigit() or int(d) in mine:
            continue
        try:
            cmd = open(f"/proc/{d}/cmdline", "rb").read().replace(b"\0", b" ")
            stat = open(f"/proc/{d}/stat").read()
        except OSError:
            continue
        if marker.encode() in cmd:
            found.append(d)
            parent[d] = stat.rsplit(")", 1)[1].split()[1]
    # A launcher that stays resident carries the same argv as the thing it
    # launched — `sudo kevy --dir X` matches every marker `kevy --dir X` does,
    # and counting both reads as a leaked server. The launcher is by
    # definition the parent, so drop any match that another match descends
    # from. No name list: this holds for sudo, a shell, or anything else.
    kids = set(parent.values())
    return [d for d in found if d not in kids]


def proc_tree_rss(marker):
    """Σ RSS of every process whose cmdline carries `marker`. Containers
    share the host process table, so this reaches into the PG container
    without a docker socket — and the marker keeps it off the other
    Postgres instances this box happens to run."""
    total = 0
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                if marker.encode() not in f.read():
                    continue
            with open(f"/proc/{pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        total += int(line.split()[1]) * 1024
                        break
        except (OSError, ValueError):
            continue
    return total


def report(engine, mode, csv_path, load_s, lat, disk_b, rss_b, rows, extra=None):
    mb = csv_mb(csv_path)
    out = {
        "engine": engine, "mode": mode, "rows": rows,
        "csv_mb": round(mb, 1),
        "load_seconds": round(load_s, 2),
        "load_rows_per_s": int(rows / load_s) if load_s else 0,
        "load_mb_per_s": round(mb / load_s, 1) if load_s else 0,
        "disk_bytes": disk_b, "disk_per_csv_mb_kb": round(disk_b / mb / 1024, 1),
        "rss_bytes": rss_b, "rss_per_csv_mb_kb": round(rss_b / mb / 1024, 1),
    }
    if extra:
        out.update(extra)
    for k, xs in lat.items():
        if xs:
            out[f"{k}_p50_us"] = int(pct(xs, 0.50))
            out[f"{k}_p99_us"] = int(pct(xs, 0.99))
            out[f"{k}_mean_us"] = int(statistics.fmean(xs))
    print(json.dumps(out))


# ── postgres ────────────────────────────────────────────────────────────────

def run_pg():
    import psycopg

    csv_path, dsn = opt("--csv"), opt("--dsn")
    n = int(opt("--samples", "5000"))
    container = opt("--container", "kevy-pgcmp")
    rows = sum(1 for _ in open(csv_path))

    with psycopg.connect(dsn, autocommit=True) as c:
        c.execute("DROP TABLE IF EXISTS t")
        c.execute("""CREATE TABLE t (
            id BIGINT PRIMARY KEY, name TEXT, dept TEXT,
            age INT, ts BIGINT, sku BIGINT, pad TEXT)""")
        # The same two access paths kevy declares: a range index on age
        # (with the columns a list page shows) and a composite dept+age.
        t0 = time.time()
        with open(csv_path) as f, c.cursor().copy(
            "COPY t (id, name, dept, age, ts, sku, pad) FROM STDIN WITH (FORMAT csv)"
        ) as cp:
            while chunk := f.read(1 << 20):
                cp.write(chunk)
        c.execute("CREATE INDEX t_sku ON t (sku)")
        c.execute("CREATE INDEX t_dept_ts ON t (dept, ts)")
        c.execute("ANALYZE t")
        load_s = time.time() - t0

        c.execute("CHECKPOINT")
        cur = c.cursor()
        lat = time_serial(cur, PG_SHAPES, n, rows)

        c.execute("CHECKPOINT")
        # Ask Postgres itself rather than shelling into the container: the
        # bench account has no docker socket, and the SQL answer is the
        # authoritative one anyway (heap + indexes + TOAST + FSM).
        cur.execute("SELECT pg_total_relation_size('t')")
        disk = int(cur.fetchone()[0])
        cur.execute("SELECT pg_database_size(current_database())")
        db_bytes = int(cur.fetchone()[0])

    # Postgres is a process tree. The container shares the host's process
    # table, so sum every backend of THIS cluster — identified by the
    # cluster_name the runner passes in, since the box hosts other PGs.
    rss = proc_tree_rss(opt("--cluster", "kevypgcmp"))
    report("postgres18", "stock", csv_path, load_s, lat, disk, rss, rows,
           extra={"db_bytes": db_bytes,
                  "db_per_csv_mb_kb": round(db_bytes / csv_mb(csv_path) / 1024, 1)})


# ── kevy ────────────────────────────────────────────────────────────────────

# ── the four shapes, defined once ────────────────────────────────────
#
# Both the serial runs below and the concurrency sweep in pgconc.py call
# these, so "the idx query" cannot come to mean two different queries
# depending on which axis is being measured. Each takes an open handle, a
# seeded RNG and the row count, and performs exactly one operation.

def pg_pk(cur, rng, rows):
    cur.execute("SELECT name, dept, age FROM t WHERE id = %s", (rng.randrange(rows),))
    cur.fetchall()


def pg_idx(cur, rng, rows):
    k = rng.randrange(max(1, rows // 20))
    cur.execute("SELECT id, name FROM t WHERE sku = %s LIMIT 20", (k,))
    cur.fetchall()


def pg_page(cur, rng, rows):
    # A random time window anywhere in the table, not a fixed
    # low-cardinality slice: the page a real user asks for.
    d = DEPTS[rng.randrange(len(DEPTS))]
    lo = 1700000000 + rng.randrange(max(1, rows - 2000))
    cur.execute(
        "SELECT id, name, ts FROM t WHERE dept = %s AND ts BETWEEN %s AND %s "
        "ORDER BY ts LIMIT 20", (d, lo, lo + 2000))
    cur.fetchall()


def pg_write(cur, rng, rows):
    cur.execute("UPDATE t SET age = age + 1 WHERE id = %s", (rng.randrange(rows),))


def k_pk(c, rng, rows):
    c.cmd("HMGET", f"row:{rng.randrange(rows)}", "name", "dept", "age")


def k_idx(c, rng, rows):
    # FIELDS name, because the PostgreSQL side selects id and name. Without
    # it kevy returned the key and the indexed value and no columns at all,
    # so the two engines were not answering the same question and every read
    # ratio taken before 2026-08-23 flattered kevy by the cost of the columns
    # it never fetched. `name` is a declared VALUES column on t.sku.
    k = rng.randrange(max(1, rows // 20))
    c.cmd("IDX.QUERY", "t.sku", "EQ", str(k), "LIMIT", "20", "FIELDS", "name")


def k_page(c, rng, rows):
    d = DEPTS[rng.randrange(len(DEPTS))]
    lo = 1700000000 + rng.randrange(max(1, rows - 2000))
    # Same correction: the PostgreSQL side selects id, name and ts. The key
    # carries id, the composite value carries ts, so `name` is what was
    # missing from the reply.
    c.cmd("IDX.QUERY", "t.by_dept_ts", "WHERE", "dept", "EQ", d,
          "RANGE", "ts", str(lo), str(lo + 2000), "LIMIT", "20", "FIELDS", "name")


def k_write(c, rng, rows):
    c.cmd("HSET", f"row:{rng.randrange(rows)}", "age", str(18 + rng.randrange(60)))


PG_SHAPES = {"pk": pg_pk, "idx": pg_idx, "page": pg_page, "write": pg_write}
K_SHAPES = {"pk": k_pk, "idx": k_idx, "page": k_page, "write": k_write}
SHAPE_ORDER = ("pk", "idx", "page", "write")


def time_serial(handle, shapes, n, rows, seed=7):
    """Run each shape n times on one handle, returning µs per operation."""
    lat = {}
    rng = random.Random(seed)
    for name in SHAPE_ORDER:
        fn, xs = shapes[name], []
        for _ in range(n):
            t = time.perf_counter_ns()
            fn(handle, rng, rows)
            xs.append((time.perf_counter_ns() - t) / 1000)
        lat[name] = xs
    return lat


def enc(*parts):
    out = [b"*%d\r\n" % len(parts)]
    for p in parts:
        b = p if isinstance(p, bytes) else str(p).encode()
        out.append(b"$%d\r\n%s\r\n" % (len(b), b))
    return b"".join(out)


class K:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=600)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.f = self.s.makefile("rb")

    def reply(self):
        line = self.f.readline()
        if not line:
            raise EOFError
        t, body = line[:1], line[1:-2]
        if t == b"$":
            n = int(body)
            return None if n < 0 else self.f.read(n + 2)[:-2]
        if t == b"*":
            n = int(body)
            return [] if n < 0 else [self.reply() for _ in range(n)]
        return line

    def cmd(self, *parts):
        self.s.sendall(enc(*parts))
        return self.reply()


def run_kevy():
    csv_path, port = opt("--csv"), int(opt("--port"))
    n = int(opt("--samples", "5000"))
    mode = opt("--mode", "none")
    datadir = opt("--datadir", "")
    rows = sum(1 for _ in open(csv_path))
    c = K(port)

    # Same shape as the PG schema: typed columns, a range index on age
    # carrying the list-page columns, and a composite dept+age path.
    # Encode the whole load OUTSIDE the timer. psycopg streams a CSV that
    # already exists on disk into COPY, so PG's number is server-side
    # ingest; building RESP frames in Python inside the timer would have
    # measured this script instead of kevy. Both sides now stream
    # pre-formatted bytes and the clock covers the server's work.
    frames, chunk, chunks = [], [], []
    with open(csv_path) as f:
        for line in csv.reader(f):
            i, name, dept, age, ts, sku, pad = line
            chunk.append(enc("HSET", f"row:{i}", "id", i, "name", name, "dept", dept,
                             "age", age, "ts", ts, "sku", sku, "pad", pad))
            if len(chunk) == 200:
                chunks.append((b"".join(chunk), 200)); chunk = []
    if chunk:
        chunks.append((b"".join(chunk), len(chunk)))
    del frames

    t0 = time.time()
    for payload, count in chunks:
        c.s.sendall(payload)
        for _ in range(count):
            c.reply()
    c.cmd("TABLE.DECLARE", "t", "PREFIX", "row:", "PK", "id",
          "COLUMN", "id", "i64", "COLUMN", "name", "str", "COLUMN", "dept", "str",
          "COLUMN", "age", "i64", "COLUMN", "ts", "i64", "COLUMN", "sku", "i64",
          "INDEX", "sku", "range", "VALUES", "name",
          "ORDERPATH", "by_dept_ts", "ON", "dept", "THEN", "ts")
    for probe in (("IDX.QUERY", "t.sku", "EQ", "1"),
                  ("IDX.QUERY", "t.by_dept_ts", "WHERE", "dept", "EQ", "eng", "LIMIT", "1")):
        for _ in range(3600):
            r = c.cmd(*probe)
            if not (isinstance(r, bytes) and r.startswith(b"-INDEXBUILDING")):
                break
            time.sleep(1)
    load_s = time.time() - t0

    # The two engines must answer the same question. The PostgreSQL read
    # shapes select columns; for a whole release line the kevy shapes did not
    # ask for any, so kevy returned a key and an indexed value and was timed
    # doing strictly less work. Nothing in the output said so. Assert that a
    # read shape comes back carrying the column it is supposed to carry.
    for label, probe in (("idx", ("IDX.QUERY", "t.sku", "EQ", "1", "LIMIT", "20", "FIELDS", "name")),
                         ("page", ("IDX.QUERY", "t.by_dept_ts", "WHERE", "dept", "EQ", "eng",
                                   "RANGE", "ts", "0", "9999999999", "LIMIT", "20",
                                   "FIELDS", "name"))):
        r = c.cmd(*probe)
        blob = repr(r).encode() if not isinstance(r, bytes) else r
        if b"user" not in blob:
            sys.exit(f"pgcompare: REFUSED — the {label} shape came back without the "
                     f"hydrated column; it would be timed doing less work than the "
                     f"SQL it is compared against. Reply: {blob[:200]!r}")

    lat = time_serial(c, K_SHAPES, n, rows)

    # A "tiered" row nobody can check is worth nothing: pull the gauges so
    # the finding can show demotion actually happened and what stayed
    # resident (indexes are RAM-resident by design).
    extra = {}
    if mode.startswith("tier"):
        info = c.cmd("INFO", "tiering")
        text = info.decode(errors="replace") if isinstance(info, bytes) else ""
        for line in text.splitlines():
            if ":" in line and line.split(":")[0] in (
                "tier_budget_bytes", "tier_effective_target", "cold_keys",
                "cold_bytes", "stub_bytes", "index_reserved_bytes",
                "vlog_size_bytes", "promotions_total", "demotions_total",
            ):
                k, v = line.split(":", 1)
                try:
                    extra[k] = int(v.strip())
                except ValueError:
                    pass
        um = c.cmd("INFO", "memory")
        umt = um.decode(errors="replace") if isinstance(um, bytes) else ""
        for line in umt.splitlines():
            if line.startswith("used_memory:"):
                extra["used_memory"] = int(line.split(":", 1)[1].strip())

    time.sleep(3)  # let the AOF settle before sizing it
    disk = 0
    if datadir:
        for root, _, files in os.walk(datadir):
            for fn in files:
                disk += os.path.getsize(os.path.join(root, fn))
    pid = server_pids(datadir or f"--port {port}")
    if len(pid) != 1:
        sys.exit(f"pgcompare: REFUSED — {len(pid)} server(s) own {datadir!r}. "
                 f"One is the measurement; zero means the memory column would "
                 f"be a lie; more means a mode leaked and shares the port.")
    rss = 0
    with open(f"/proc/{pid[0]}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                rss = int(line.split()[1]) * 1024
    # The label was "kevy4" as a constant, so every result file since the 4.x
    # line has said kevy4 whatever binary produced it — a 5.3.0 run reported
    # itself as kevy4. Ask the server what it is.
    ver = ""
    srv = K(port).cmd("INFO", "server")
    for line in (srv.decode(errors="replace") if isinstance(srv, bytes) else "").splitlines():
        if line.startswith("kevy_version:"):
            ver = line.split(":", 1)[1].strip()
    report(f"kevy{ver}" if ver else "kevy", mode, csv_path, load_s, lat, disk, rss, rows,
           extra=extra or None)


CMDS = {"gen": run_gen, "pg": run_pg, "kevy": run_kevy}

# Guarded so pgconc.py can import the shapes rather than restate them.
if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] not in CMDS:
        sys.exit(__doc__)
    CMDS[sys.argv[1]]()
