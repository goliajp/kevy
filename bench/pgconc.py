#!/usr/bin/env python3
"""The concurrency axis: the same four shapes under 1 / 8 / 32 / 64 clients.

WHY THIS EXISTS. pgcompare.py drives one connection and sends one command
at a time, so it answers "how long does a query take when nothing else is
happening". That is a latency question, and it is the shape least
favourable to a thread-per-core busy-poll engine — every shard fans out
for a single client that is sitting and waiting. It is equally unfavourable
to PostgreSQL, which is built to overlap many backends. Neither engine is
being asked the question that "can this replace an RDS" actually rests on:
what happens when sixty-four clients arrive together.

WHY PROCESSES, NOT THREADS. One CPython interpreter cannot honestly drive
64 concurrent sockets; the GIL would serialise the driver and report its
own contention as the engine's latency. Each client is its own process
with its own connection.

WHY THE DRIVER'S CPU IS IN EVERY ROW. A saturated driver produces numbers
that look exactly like a saturated server — the same flattening curve, the
same rising p99. The only way to tell them apart is to measure the driver
too, so `driver_cores` is reported beside every result and a run that
approaches its core allotment says so in the row rather than in a footnote
nobody reads.

  bench/pgconc.py pg   --dsn DSN  --rows N --conc 1,8,32,64 [--ops 400]
  bench/pgconc.py kevy --port P   --rows N --conc 1,8,32,64 [--ops 400]
                       --mode NAME --label NAME

Shapes, rows and RNG discipline come from pgcompare.py, so "the idx query"
cannot mean two different queries depending on which axis is measured.
"""
import json
import multiprocessing as mp
import os
import random
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pgcompare import K, K_SHAPES, PG_SHAPES, SHAPE_ORDER, opt, pct  # noqa: E402

WARMUP = 5


def _open(engine, target):
    """A connection plus the shape table that drives it."""
    if engine == "pg":
        import psycopg
        conn = psycopg.connect(target, autocommit=True)
        return conn.cursor(), PG_SHAPES, conn.close
    client = K(int(target))
    return client, K_SHAPES, lambda: None


def _worker(engine, target, rows, ops, wid, barrier, q, want):
    """One client. Every shape starts on the barrier so the load is real."""
    handle, shapes, close = _open(engine, target)
    rng = random.Random(1000 + wid)
    out = {}
    for name in want:
        fn = shapes[name]
        for _ in range(WARMUP):
            fn(handle, rng, rows)
        barrier.wait()
        cpu0, t0, xs = time.process_time(), time.perf_counter(), []
        for _ in range(ops):
            t = time.perf_counter_ns()
            fn(handle, rng, rows)
            xs.append((time.perf_counter_ns() - t) / 1000)
        out[name] = (t0, time.perf_counter(), time.process_time() - cpu0, xs)
    close()
    q.put((wid, out))


def _collect(engine, target, rows, ops, conc, want):
    """Run `conc` clients; return {shape: (starts, ends, cpu, latencies)}."""
    barrier, q = mp.Barrier(conc), mp.Queue()
    procs = [mp.Process(target=_worker,
                        args=(engine, target, rows, ops, i, barrier, q, want))
             for i in range(conc)]
    for p in procs:
        p.start()
    got = [q.get() for _ in procs]
    for p in procs:
        p.join()
    dead = [p.exitcode for p in procs if p.exitcode]
    if dead:
        sys.exit(f"pgconc: {len(dead)} client(s) died (exit {dead[0]}) — "
                 f"the level would be reported from survivors only")
    merged = {}
    for name in want:
        starts, ends, cpu, xs = [], [], 0.0, []
        for _, out in got:
            t0, t1, c, lat = out[name]
            starts.append(t0); ends.append(t1); cpu += c; xs.extend(lat)
        merged[name] = (min(starts), max(ends), cpu, xs)
    return merged


def _row(label, mode, conc, ops, rows, merged, cores, want):
    """One result line: latency, throughput, and what the driver cost."""
    rec = {"engine": label, "mode": mode, "conc": conc,
           "ops_per_conn": ops, "rows": rows}
    hot = []
    for name in want:
        t0, t1, cpu, xs = merged[name]
        wall = max(t1 - t0, 1e-9)
        drv = cpu / wall
        hot.append(drv)
        rec[f"{name}_p50_us"] = round(pct(xs, 0.50))
        rec[f"{name}_p99_us"] = round(pct(xs, 0.99))
        rec[f"{name}_ops_per_s"] = round(len(xs) / wall)
        # Latency and throughput are two views of the same ops, so they have
        # to agree: mean latency x ops should be the wall time / conc. When
        # they do not, one of them is measuring the wrong thing — a p50 that
        # was silently the maximum got this far once.
        rec[f"{name}_mean_us"] = round(sum(xs) / len(xs))
        rec[f"{name}_driver_cores"] = round(drv, 2)
    rec["driver_cores_max"] = round(max(hot), 2)
    rec["driver_cores_budget"] = cores
    # Not a footnote: past ~80% of the cores the driver was given, the
    # curve that follows is the driver's, and the row says so itself.
    rec["driver_saturated"] = max(hot) >= 0.8 * cores
    return rec


def main():
    engine = sys.argv[1]
    target = opt("--dsn") if engine == "pg" else opt("--port")
    rows = int(opt("--rows"))
    ops = int(opt("--ops", "400"))
    label = opt("--label", "postgres18" if engine == "pg" else "kevy")
    mode = opt("--mode", "stock")
    cores = int(opt("--driver-cores", str(os.cpu_count() or 1)))
    # --shapes narrows the run to one or more shapes. The fsync probe needs a
    # window that is nothing but writes, so a counter attached to it is
    # counting that shape and not the three that precede it.
    want = tuple(opt("--shapes", ",".join(SHAPE_ORDER)).split(","))
    bad = [s for s in want if s not in SHAPE_ORDER]
    if bad:
        sys.exit(f"pgconc: unknown shape(s) {bad}; known: {list(SHAPE_ORDER)}")
    for conc in [int(x) for x in opt("--conc", "1,8,32,64").split(",")]:
        merged = _collect(engine, target, rows, ops, conc, want)
        print(json.dumps(_row(label, mode, conc, ops, rows, merged, cores, want)),
              flush=True)


if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] not in ("pg", "kevy"):
        sys.exit(__doc__)
    main()
