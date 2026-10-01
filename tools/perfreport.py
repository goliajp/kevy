#!/usr/bin/env python3
"""The arithmetic behind bench/perfgate.sh and bench/arena.sh.

Pure functions only; the bench scripts collect the raw facts and do the I/O.

A window record carries deltas taken across one measurement window:
  cpu        {cpu: [busy_ticks, total_ticks]} from /proc/stat
  srv_ticks  utime + stime of the server process
  gens       [[ticks, threads], ...] one entry per load generator
  cmds       total_commands_processed delta, wall_ns the wall time it spans
  secs       the hardware-counter window, perf the counters (or None)
  srv_cpus   the cpus the server was pinned to
  value_us   instead of all the above, the latency angle's p95

An observation is one side (A or B) of one angle in one round: the mean of
its windows. A compare is N rounds of paired observations; every metric is
judged on the per-round ratios B / A, their median and their spread.
"""

import math
import random
import statistics

PERF_EVENTS = ("instructions:u", "instructions:k", "cycles", "task-clock",
               "raw_syscalls:sys_enter")
# a counter the box cannot read leaves its column empty instead of the run
OPTIONAL_EVENTS = ("raw_syscalls:sys_enter",)

# metric -> (label, "cost" when higher is worse, "rate" when lower is worse)
METRICS = {
    "ops": ("ops/s", "rate"),
    "instr": ("instr/op", "cost"),
    "instr_u": ("instr_u/op", "cost"),
    "instr_k": ("instr_k/op", "cost"),
    "cycles": ("cyc/op", "cost"),
    "sys": ("sys/op", "cost"),
    "p95_us": ("p95 µs", "cost"),
}


def cpu_list(spec):
    """'0-3,8,12-15' -> [0, 1, 2, 3, 8, 12, 13, 14, 15]"""
    out = []
    for part in spec.split(","):
        if "-" in part:
            lo, hi = part.split("-")
            out.extend(range(int(lo), int(hi) + 1))
        elif part:
            out.append(int(part))
    return out


def parse_proc_stat(text):
    """Per-CPU (busy, total) ticks; idle and iowait are the only non-busy."""
    out = {}
    for line in text.splitlines():
        f = line.split()
        if not f or not f[0].startswith("cpu") or f[0] == "cpu":
            continue
        v = [int(x) for x in f[1:9]]
        total = sum(v)
        out[int(f[0][3:])] = (total - v[3] - v[4], total)
    return out


def proc_stat_delta(before, after):
    return {c: [after[c][0] - before[c][0], after[c][1] - before[c][1]]
            for c in after if c in before}


def _event_name(field):
    """'armv8_cortex_a76/instructions:u/' and 'instructions:u' -> 'instructions:u'.

    On a big.LITTLE box perf prints one line per core type; the pmu prefix
    is dropped here and the lines are summed by the caller."""
    return field.split("/")[1] if field.count("/") >= 2 else field


def parse_perfstat(text):
    """`perf stat -x,` lines -> {event: value}, or None when a required
    counter is missing. A counter a core type did not count ('<not
    counted>') adds nothing; one the box does not support at all is absent,
    which only the optional events may be."""
    got = {}
    for line in text.splitlines():
        f = line.split(",")
        if len(f) < 3 or line.startswith("#"):
            continue
        name = _event_name(f[2])
        if name not in PERF_EVENTS:
            continue
        if f[0].startswith("<"):
            got.setdefault(name, None)
            continue
        got[name] = (got.get(name) or 0.0) + float(f[0])
    if any(got.get(e) is None for e in PERF_EVENTS if e not in OPTIONAL_EVENTS):
        return None
    return got


def _frac(ticks, total):
    return ticks / total if total else 0.0


def window_lines(w):
    """The metrics of one window, plus its load facts.

    The op count spans the INFO bracket (wall_ns) and the counters span the
    perf window (secs); both become rates before dividing, so the few
    milliseconds between the two brackets do not bias the per-op numbers."""
    if "value_us" in w:
        return {"p95_us": float(w["value_us"])}
    ops = w["cmds"] / (w["wall_ns"] / 1e9)
    out = {"ops": ops}
    cpu = {int(k): v for k, v in w["cpu"].items()}
    srv = cpu_list(w["srv_cpus"])
    per_cpu = sum(cpu[c][1] for c in srv) / len(srv)
    gen_ticks = sum(g[0] for g in w.get("gens") or [])
    threads = sum(g[1] for g in w.get("gens") or [])
    busy = sum(v[0] for v in cpu.values())
    total = sum(v[1] for v in cpu.values())
    out["foreign"] = max(0.0, _frac(busy - w["srv_ticks"] - gen_ticks, total))
    out["client"] = _frac(gen_ticks, per_cpu * threads) if threads else 0.0
    perf = w.get("perf")
    if perf:
        def per_op(e):
            v = perf.get(e)
            return None if v is None else v / w["secs"] / ops
        out["instr_u"] = per_op("instructions:u")
        out["instr_k"] = per_op("instructions:k")
        out["instr"] = out["instr_u"] + out["instr_k"]
        out["cycles"] = per_op("cycles")
        out["sys"] = per_op("raw_syscalls:sys_enter")
        out["util"] = perf["task-clock"] / 1000.0 / w["secs"] / len(srv)
    return out


def observation(windows):
    """Mean of one side's windows; 'foreign' and 'client' take the worst."""
    per = [window_lines(w) for w in windows]
    out = {}
    for k in per[0]:
        vals = [p[k] for p in per if p.get(k) is not None]
        if not vals:
            out[k] = None
        elif k in ("foreign", "client"):
            out[k] = max(vals)
        else:
            out[k] = statistics.fmean(vals)
    return out


def ratios(pairs, metric):
    """Per-round B / A of one metric; rounds missing it on either side drop."""
    out = []
    for a, b in pairs:
        va, vb = a.get(metric), b.get(metric)
        if va and vb is not None:
            out.append(vb / va)
    return out


def spread(rs):
    """Half the range of the per-round ratios: how far a round can sit from
    the middle. With three rounds it is the only honest width there is."""
    return (max(rs) - min(rs)) / 2 if len(rs) > 1 else 0.0


def worse(metric, r, limit):
    """Is ratio r beyond the limit in the bad direction for this metric?"""
    return r < limit if METRICS[metric][1] == "rate" else r > limit


def verdict(metric, rs, limit, noise):
    """ok | FAIL | NOISY for one metric of one angle.

    Every round beyond the limit fails and every round inside it passes, at
    any spread. Rounds on both sides of the limit are judged on their median
    only when the spread is inside the stated noise bound; otherwise the
    answer is to run it again."""
    if not rs:
        return "—"
    beyond = [worse(metric, r, limit) for r in rs]
    if all(beyond):
        return "FAIL"
    if not any(beyond):
        return "ok"
    if spread(rs) > noise:
        return "NOISY"
    return "FAIL" if worse(metric, statistics.median(rs), limit) else "ok"


def summarize(pairs, metric):
    rs = ratios(pairs, metric)
    if not rs:
        return None
    return {"median": statistics.median(rs), "spread": spread(rs), "rounds": rs}


def fmt_ratio(s):
    return "—" if s is None else f"{s['median']:.3f} ±{s['spread'] * 100:.1f}%"


def fmt_abs(v, metric):
    if v is None:
        return "—"
    if metric == "ops":
        return f"{v / 1e6:.2f}M"
    if metric == "sys":
        return f"{v:.3f}"
    return f"{v:.0f}"


def bootstrap_ratio(a, b, level=0.99, iters=10000, seed=1):
    """Paired bootstrap interval of mean(a) / mean(b).

    a[i] and b[i] were taken in the same round and slot, so they are resampled
    together: whatever moved the box in that slot moves both.
    """
    if len(a) != len(b) or not a:
        raise ValueError("paired samples must be non-empty and equal length")
    rng = random.Random(seed)
    n = len(a)
    rs = []
    for _ in range(iters):
        idx = [rng.randrange(n) for _ in range(n)]
        rs.append(sum(a[i] for i in idx) / sum(b[i] for i in idx))
    rs.sort()
    tail = (1 - level) / 2
    lo = rs[int(math.floor(tail * (iters - 1)))]
    hi = rs[int(math.ceil((1 - tail) * (iters - 1)))]
    return lo, hi
