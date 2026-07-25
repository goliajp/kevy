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
    pad = "x" * pad_len
    t0 = time.time()
    with open(out, "w", newline="") as f:
        w = csv.writer(f)
        for i in range(rows):
            w.writerow([
                i,
                f"user{i:08d}",
                DEPTS[i % len(DEPTS)],
                18 + (i % 60),
                1700000000 + i,
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
            age INT, ts BIGINT, pad TEXT)""")
        # The same two access paths kevy declares: a range index on age
        # (with the columns a list page shows) and a composite dept+age.
        t0 = time.time()
        with open(csv_path) as f, c.cursor().copy(
            "COPY t (id, name, dept, age, ts, pad) FROM STDIN WITH (FORMAT csv)"
        ) as cp:
            while chunk := f.read(1 << 20):
                cp.write(chunk)
        c.execute("CREATE INDEX t_age ON t (age)")
        c.execute("CREATE INDEX t_dept_age ON t (dept, age)")
        c.execute("ANALYZE t")
        load_s = time.time() - t0

        c.execute("CHECKPOINT")
        lat = {"pk": [], "idx": [], "page": [], "write": []}
        rng = random.Random(7)
        cur = c.cursor()
        for _ in range(n):
            i = rng.randrange(rows)
            t = time.perf_counter_ns()
            cur.execute("SELECT name, dept, age FROM t WHERE id = %s", (i,))
            cur.fetchall()
            lat["pk"].append((time.perf_counter_ns() - t) / 1000)
        for _ in range(n):
            a = 18 + rng.randrange(60)
            t = time.perf_counter_ns()
            cur.execute("SELECT id, name FROM t WHERE age = %s LIMIT 20", (a,))
            cur.fetchall()
            lat["idx"].append((time.perf_counter_ns() - t) / 1000)
        for _ in range(n):
            d = DEPTS[rng.randrange(len(DEPTS))]
            a = 18 + rng.randrange(40)
            t = time.perf_counter_ns()
            cur.execute(
                "SELECT id, name, age FROM t WHERE dept = %s AND age BETWEEN %s AND %s "
                "ORDER BY age LIMIT 20", (d, a, a + 20))
            cur.fetchall()
            lat["page"].append((time.perf_counter_ns() - t) / 1000)
        for _ in range(n):
            i = rng.randrange(rows)
            t = time.perf_counter_ns()
            cur.execute("UPDATE t SET age = age + 1 WHERE id = %s", (i,))
            lat["write"].append((time.perf_counter_ns() - t) / 1000)

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
            i, name, dept, age, ts, pad = line
            chunk.append(enc("HSET", f"row:{i}", "id", i, "name", name, "dept", dept,
                             "age", age, "ts", ts, "pad", pad))
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
          "COLUMN", "age", "i64", "COLUMN", "ts", "i64",
          "INDEX", "age", "range", "VALUES", "name", "dept",
          "ORDERPATH", "by_dept_age", "ON", "dept", "THEN", "age")
    for probe in (("IDX.QUERY", "t.age", "EQ", "20"),
                  ("IDX.QUERY", "t.by_dept_age", "WHERE", "dept", "EQ", "eng", "LIMIT", "1")):
        for _ in range(3600):
            r = c.cmd(*probe)
            if not (isinstance(r, bytes) and r.startswith(b"-INDEXBUILDING")):
                break
            time.sleep(1)
    load_s = time.time() - t0

    lat = {"pk": [], "idx": [], "page": [], "write": []}
    rng = random.Random(7)
    for _ in range(n):
        i = rng.randrange(rows)
        t = time.perf_counter_ns()
        c.cmd("HMGET", f"row:{i}", "name", "dept", "age")
        lat["pk"].append((time.perf_counter_ns() - t) / 1000)
    for _ in range(n):
        a = 18 + rng.randrange(60)
        t = time.perf_counter_ns()
        c.cmd("IDX.QUERY", "t.age", "EQ", str(a), "LIMIT", "20")
        lat["idx"].append((time.perf_counter_ns() - t) / 1000)
    for _ in range(n):
        d = DEPTS[rng.randrange(len(DEPTS))]
        a = 18 + rng.randrange(40)
        t = time.perf_counter_ns()
        c.cmd("IDX.QUERY", "t.by_dept_age", "WHERE", "dept", "EQ", d,
              "RANGE", "age", str(a), str(a + 20), "LIMIT", "20")
        lat["page"].append((time.perf_counter_ns() - t) / 1000)
    for _ in range(n):
        i = rng.randrange(rows)
        t = time.perf_counter_ns()
        c.cmd("HSET", f"row:{i}", "age", str(18 + rng.randrange(60)))
        lat["write"].append((time.perf_counter_ns() - t) / 1000)

    time.sleep(3)  # let the AOF settle before sizing it
    disk = 0
    if datadir:
        for root, _, files in os.walk(datadir):
            for fn in files:
                disk += os.path.getsize(os.path.join(root, fn))
    pid = subprocess.run(["pgrep", "-f", f"kevy --port {port}"],
                         capture_output=True, text=True).stdout.split()
    rss = 0
    if pid:
        with open(f"/proc/{pid[0]}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    rss = int(line.split()[1]) * 1024
    report("kevy4", mode, csv_path, load_s, lat, disk, rss, rows)


CMDS = {"gen": run_gen, "pg": run_pg, "kevy": run_kevy}
if len(sys.argv) < 2 or sys.argv[1] not in CMDS:
    sys.exit(__doc__)
CMDS[sys.argv[1]]()
