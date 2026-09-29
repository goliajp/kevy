#!/usr/bin/env python3
"""arena's tables from its window samples.

  arena_table.py run TOPO SAMPLES            # one run: throughput + cost tables
  arena_table.py median TOPO ANCHORS S1 S2.. # clean runs: medians + paired intervals

A window sample is one line of perfgate2_window.py output: angle = the verb,
side = the engine, obs = 1 for a cell window and 0 for the headroom probe.

run exits 3 when the round is dirty (a kept window had more than 10% foreign
load on the box): its throughput table is not a result, arena-median retakes
it. median exits 0 when every cell's 99% interval of kevy / other lies above
1 for every competitor, 1 when some cell's does not, 2 on no samples.
"""

import collections
import json
import pathlib
import statistics
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "tools"))
import perfstats as ps  # noqa: E402
import perfwindow as pw  # noqa: E402

VERBS = ["GET", "SET", "INCR", "SADD", "HSET", "LPUSH", "ZADD"]
ENGINES = ["kevy", "redis8", "valkey", "dragonfly"]
# a probe with twice the load threads that lands this far above the best of
# the cell's own windows means the load generator, not the engine, set the
# cell; above the best window, not the median, so window-to-window spread
# alone cannot trip it
PROBE_GAIN_MAX = 1.02


def load(path):
    return [json.loads(line) for line in pathlib.Path(path).read_text().splitlines() if line.strip()]


def cells(windows, topo):
    """{(engine, verb): {'win': [lines...], 'probe': lines or None}}"""
    out = collections.defaultdict(lambda: {"win": [], "probe": None, "flags": []})
    for w in windows:
        c = out[(w["side"], w["angle"])]
        lines = pw.window_lines(w, topo)
        if w["obs"] == 0:
            c["probe"] = lines
        else:
            c["win"].append(lines)
            c["flags"].append(w["classify"])
    return out


def client_bound(c):
    best = max(x["T.ops"] for x in c["win"])
    return c["probe"] is not None and c["probe"]["T.ops"] > best * PROBE_GAIN_MAX


def cmd_run(topo, samples):
    table = cells(load(samples), topo)
    dirty = [k for k, c in table.items() if not all(f["t_ok"] for f in c["flags"])]
    for (engine, verb), c in sorted(table.items()):
        t = [x["T.ops"] for x in c["win"]]
        sd = statistics.stdev(t) if len(t) > 1 else 0.0
        print(f"{engine} {verb} {statistics.median(t):.0f} {sd:.0f}")
    print("\n# cost per op (same windows): engine verb instr_u instr_k syscalls cycles engine_cpus")
    for (engine, verb), c in sorted(table.items()):
        m = {k: statistics.fmean(x[k] for x in c["win"])
             for k in ("C.instr_u", "C.instr_k", "C.sys", "S.cyc", "S.util")}
        print(f"cost {engine} {verb} {m['C.instr_u']:.0f} {m['C.instr_k']:.0f} "
              f"{m['C.sys']:.3f} {m['S.cyc']:.0f} {m['S.util'] * len(pw.cpu_list(topo['srv_cpus'])):.2f}")
    for (engine, verb), c in sorted(table.items()):
        if client_bound(c):
            print(f"# CLIENT-BOUND {engine} {verb}: {PROBE_THREADS_NOTE}")
    if dirty:
        print("# DIRTY round: foreign load above 10% during " + ", ".join(f"{e} {v}" for e, v in sorted(dirty)))
        return 3
    return 0


PROBE_THREADS_NOTE = "twice the load threads beat its best window by more than 2% — not a result"


def run_cells(paths, topo):
    """Per cell and engine, the T.ops samples in (run, window) order, and
    whether any run found the cell client-bound."""
    samples = collections.defaultdict(list)
    bound = set()
    for path in paths:
        for (engine, verb), c in cells(load(path), topo).items():
            samples[(engine, verb)].extend(x["T.ops"] for x in c["win"])
            if client_bound(c):
                bound.add(verb)
    return samples, bound


def cmd_median(topo, anchors, paths):
    samples, bound = run_cells(paths, topo)
    if not samples:
        print("arena-median: no samples", file=sys.stderr)
        return 2
    pins = {k: v["pinned"] for k, v in json.loads(pathlib.Path(anchors).read_text())["anchors"].items()}
    label = {"kevy": "kevy", "redis8": f"Redis {pins['redis']}",
             "valkey": f"valkey {pins['valkey']}", "dragonfly": f"Dragonfly {pins['dragonfly']}"}
    engines = [e for e in ENGINES if any(k[0] == e for k in samples)]
    print(f"# arena-median over {len(paths)} clean runs — per-cell medians\n")
    print("| verb | " + " | ".join(label[e] for e in engines) + " |")
    print("|---|" + "---:|" * len(engines))
    for v in VERBS:
        if v in bound:
            print(f"| {v} | " + " | ".join("CLIENT-BOUND" for _ in engines) + " |")
            continue
        print(f"| {v} | " + " | ".join(f"{statistics.median(samples[(e, v)]):,.0f}" for e in engines) + " |")
    return intervals(samples, bound, engines, label)


def intervals(samples, bound, engines, label):
    print("\n## kevy / other, 99% paired bootstrap interval (lower bound = the claim that holds)\n")
    print("| verb | " + " | ".join(f"vs {label[e]}" for e in engines[1:]) + " |")
    print("|---|" + "---:|" * (len(engines) - 1))
    weak = []
    if all(v in bound for v in VERBS):
        print("\n**No cell was set by the engines: every one is CLIENT-BOUND.**")
        return 1
    for v in VERBS:
        if v in bound:
            continue
        row = []
        for e in engines[1:]:
            k, o = samples[("kevy", v)], samples[(e, v)]
            n = min(len(k), len(o))
            lo, hi = ps.bootstrap_ratio(k[:n], o[:n])
            row.append(f"{lo:.2f}x–{hi:.2f}x" + (" NOISE" if lo <= 1.0 <= hi else ""))
            if lo <= 1.0:
                weak.append(f"{v} vs {label[e]}")
        print(f"| {v} | " + " | ".join(row) + " |")
    if weak:
        print("\n**Not separated from 1 at 99%:** " + ", ".join(weak) + ".")
        return 1
    print("\n**Every cell that is not CLIENT-BOUND: kevy's lower bound is above 1 against every competitor.**")
    return 0


def main():
    cmd, topo_path = sys.argv[1], sys.argv[2]
    topo = json.loads(pathlib.Path(topo_path).read_text())["topology"]
    if cmd == "run":
        sys.exit(cmd_run(topo, sys.argv[3]))
    sys.exit(cmd_median(topo, sys.argv[3], sys.argv[4:]))


if __name__ == "__main__":
    main()
