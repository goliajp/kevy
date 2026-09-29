#!/usr/bin/env python3
"""One measurement window of the perf gate: raw facts in, verdict inputs out.

Pure functions; bench/perfgate2_window.py collects the raw facts on the box.

A window record carries deltas taken across the window:
  cpu        {cpu: [busy_ticks, total_ticks]} from /proc/stat
  srv_ticks  utime + stime of the server process
  gens       [[ticks, threads], ...] one entry per load generator
  cmds       total_commands_processed delta, wall_ns the wall time it spans
  secs       the hardware-counter window, perf the counters (or None)
  value_us   the latency angle's number, when the window is a latency run
"""

import math

# per-sample refusal thresholds (fractions)
SIBLING_BUSY_MAX = 0.02
SERVER_FOREIGN_MAX = 0.02
CLIENT_CPU_MAX = 0.85
UTIL_MIN = 0.95
BOX_FOREIGN_MAX = 0.10
MAX_CONSECUTIVE_DISCARDS = 6

PERF_EVENTS = ("instructions:u", "instructions:k", "cycles", "task-clock",
               "raw_syscalls:sys_enter")


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


def parse_perfstat(text):
    """`perf stat -x,` lines -> {event: value}; None when a counter is absent.

    A counter that reads '<not counted>' or '<not supported>' makes the whole
    set unusable: a partial set would judge some lines on missing data.
    """
    got = {}
    for line in text.splitlines():
        f = line.split(",")
        if len(f) < 3 or line.startswith("#"):
            continue
        if f[0].startswith("<"):
            return None
        got[f[2]] = float(f[0])
    if any(e not in got for e in PERF_EVENTS):
        return None
    return got


def _frac(ticks, total):
    return ticks / total if total else 0.0


def classify(w, topo):
    """Apply the per-sample rules.

    Returns {'discard': reason or None, 'c_ok': bool, 't_ok': bool,
    'foreign': box foreign fraction}. A discarded window is retaken; c_ok
    False keeps the window but withholds the C lines; t_ok False withholds T.
    """
    cpu = {int(k): v for k, v in w["cpu"].items()}
    srv, sib = cpu_list(topo["srv_cpus"]), cpu_list(topo["idle_siblings"])
    for c in sib:
        if _frac(*cpu[c]) > SIBLING_BUSY_MAX:
            return _verdict(f"sibling cpu {c} busy {_frac(*cpu[c]):.1%}")
    srv_busy = sum(cpu[c][0] for c in srv)
    srv_total = sum(cpu[c][1] for c in srv)
    foreign_srv = _frac(srv_busy - w["srv_ticks"], srv_total)
    if foreign_srv > SERVER_FOREIGN_MAX:
        return _verdict(f"foreign load on server cpus {foreign_srv:.1%}")
    per_cpu = srv_total / len(srv)
    gens = w.get("gens") or []
    gen_ticks = sum(g[0] for g in gens)
    for ticks, threads in gens:
        if _frac(ticks, per_cpu) > CLIENT_CPU_MAX * threads:
            return _verdict("client-bound: a generator used "
                            f"{_frac(ticks, per_cpu):.2f} of {threads} threads")
    if _frac(gen_ticks, per_cpu) > CLIENT_CPU_MAX * topo["cli_threads"]:
        return _verdict("client-bound: generators used "
                        f"{_frac(gen_ticks, per_cpu):.2f} of {topo['cli_threads']} threads")
    all_busy = sum(v[0] for v in cpu.values())
    all_total = sum(v[1] for v in cpu.values())
    foreign = _frac(all_busy - w["srv_ticks"] - gen_ticks, all_total)
    util = utilisation(w, topo)
    return {"discard": None,
            "c_ok": util is not None and util >= UTIL_MIN,
            "t_ok": foreign <= BOX_FOREIGN_MAX,
            "foreign": foreign}


def _verdict(reason):
    return {"discard": reason, "c_ok": False, "t_ok": False, "foreign": None}


def utilisation(w, topo):
    perf = w.get("perf")
    if not perf:
        return None
    ncpu = len(cpu_list(w.get("srv_cpus") or topo["srv_cpus"]))
    return perf["task-clock"] / 1000.0 / w["secs"] / ncpu


def window_lines(w, topo):
    """Line values of one window.

    The op count spans the INFO bracket (wall_ns) and the counters span the
    perf window (secs); both are turned into rates before dividing, so the
    few milliseconds between the two brackets do not bias instr/op.
    """
    if "value_us" in w:
        return {"L.us": float(w["value_us"])}
    ops_rate = w["cmds"] / (w["wall_ns"] / 1e9)
    out = {"T.ops": ops_rate}
    perf = w.get("perf")
    if perf:
        per_op = lambda e: perf[e] / w["secs"] / ops_rate  # noqa: E731
        out["C.instr_u"] = per_op("instructions:u")
        out["C.instr_k"] = per_op("instructions:k")
        out["C.sys"] = per_op("raw_syscalls:sys_enter")
        out["S.cyc"] = per_op("cycles")
        out["S.util"] = utilisation(w, topo)
        out["ghz"] = perf["cycles"] / (perf["task-clock"] * 1e6)
    return out


def observation(windows, topo):
    """Mean of an instance's kept windows, plus whether C and T may judge it."""
    per = [window_lines(w, topo) for w in windows]
    flags = [classify(w, topo) for w in windows]
    lines = {k: sum(p[k] for p in per) / len(per) for k in per[0]}
    return {"lines": lines,
            "c_ok": all(f["c_ok"] for f in flags),
            "t_ok": all(f["t_ok"] for f in flags),
            "foreign": max((f["foreign"] or 0.0) for f in flags)}


def pair_lines(ref, cand, lines):
    """The lines a pair may judge: C needs both sides saturated, T needs both
    sides on a quiet box, everything else only needs the value."""
    ok = []
    for line in lines:
        if line not in ref["lines"] or line not in cand["lines"]:
            continue
        if line.startswith("C.") and not (ref["c_ok"] and cand["c_ok"]):
            continue
        if line == "T.ops" and not (ref["t_ok"] and cand["t_ok"]):
            continue
        if not (math.isfinite(ref["lines"][line]) and math.isfinite(cand["lines"][line])):
            continue
        ok.append(line)
    return ok
