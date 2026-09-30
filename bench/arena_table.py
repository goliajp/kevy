#!/usr/bin/env python3
"""arena's tables from its window samples.

  arena_table.py ANCHORS SAMPLES

A sample is one line of perfgate_measure.py output: angle = the verb,
side = the engine, obs = the round (0 for the headroom probe, whose win is
then the round), win = the window slot.

Exit 0 when every cell's 99% interval of kevy / other lies above 1 for every
competitor, 1 when some cell's does not, 2 when there are no samples.
"""

import collections
import json
import pathlib
import statistics
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "tools"))
import perfreport as pr  # noqa: E402

VERBS = ["GET", "SET", "INCR", "SADD", "HSET", "LPUSH", "ZADD"]
ENGINES = ["kevy", "redis8", "valkey", "dragonfly"]
# a probe with twice the load threads that lands this far above the best of
# the cell's own windows in that round means the load generator, not the
# engine, set the cell
PROBE_GAIN_MAX = 1.02
FOREIGN_NOTE = 0.05


def load(path):
    return [json.loads(line) for line in pathlib.Path(path).read_text().splitlines() if line.strip()]


def cells(windows):
    """{(engine, verb): {'win': {(round, slot): lines}, 'probe': {round: lines}}}"""
    out = collections.defaultdict(lambda: {"win": {}, "probe": {}})
    for w in windows:
        c = out[(w["side"], w["angle"])]
        lines = pr.window_lines(w)
        lines["ncpu"] = len(pr.cpu_list(w["srv_cpus"]))
        if w["obs"] == 0:
            c["probe"][w["win"]] = lines
        else:
            c["win"][(w["obs"], w["win"])] = lines
    return out


def client_bound(c):
    for rnd, probe in c["probe"].items():
        best = max((x["ops"] for (r, _), x in c["win"].items() if r == rnd), default=None)
        if best and probe["ops"] > best * PROBE_GAIN_MAX:
            return True
    return False


def throughput(table, engines, label, bound):
    print("| verb | " + " | ".join(label[e] for e in engines) + " |")
    print("|---|" + "---:|" * len(engines))
    for v in VERBS:
        row = ["CLIENT-BOUND" if v in bound else
               f"{statistics.median(x['ops'] for x in table[(e, v)]['win'].values()):,.0f}"
               if table[(e, v)]["win"] else "—" for e in engines]
        print(f"| {v} | " + " | ".join(row) + " |")


def intervals(table, engines, label, bound):
    print("\n## kevy / other, 99% paired bootstrap interval (lower bound = the claim that holds)\n")
    print("| verb | " + " | ".join(f"vs {label[e]}" for e in engines[1:]) + " |")
    print("|---|" + "---:|" * (len(engines) - 1))
    weak = []
    for v in VERBS:
        if v in bound:
            continue
        row = []
        for e in engines[1:]:
            k, o = table[("kevy", v)]["win"], table[(e, v)]["win"]
            slots = sorted(set(k) & set(o))
            if not slots:
                row.append("—")
                continue
            lo, hi = pr.bootstrap_ratio([k[s]["ops"] for s in slots], [o[s]["ops"] for s in slots])
            row.append(f"{lo:.2f}x–{hi:.2f}x" + (" NOISE" if lo <= 1.0 <= hi else ""))
            if lo <= 1.0:
                weak.append(f"{v} vs {label[e]}")
        print(f"| {v} | " + " | ".join(row) + " |")
    return weak


def cost(table):
    print("\n# cost per op (same windows): engine verb instr_u instr_k syscalls cycles engine_cpus fgn%max")
    for (engine, verb), c in sorted(table.items()):
        ws = list(c["win"].values())
        if not ws:
            continue
        m = {k: statistics.fmean([x[k] for x in ws if x.get(k) is not None] or [float("nan")])
             for k in ("instr_u", "instr_k", "sys", "cycles", "util")}
        print(f"cost {engine} {verb} {m['instr_u']:.0f} {m['instr_k']:.0f} {m['sys']:.3f} "
              f"{m['cycles']:.0f} {m['util'] * ws[0]['ncpu']:.2f} "
              f"{max(x['foreign'] for x in ws) * 100:.1f}")


def main():
    anchors, samples = sys.argv[1], sys.argv[2]
    table = cells(load(samples))
    if not table:
        print("arena: no samples", file=sys.stderr)
        return 2
    pins = {k: v["pinned"] for k, v in json.loads(pathlib.Path(anchors).read_text())["anchors"].items()}
    label = {"kevy": "kevy", "redis8": f"Redis {pins['redis']}",
             "valkey": f"valkey {pins['valkey']}", "dragonfly": f"Dragonfly {pins['dragonfly']}"}
    engines = [e for e in ENGINES if any(k[0] == e for k in table)]
    bound = {v for (_, v), c in table.items() if client_bound(c)}
    rounds = len({r for c in table.values() for r, _ in c["win"]})
    print(f"\n# arena over {rounds} rounds — per-cell medians of every window\n")
    throughput(table, engines, label, bound)
    weak = intervals(table, engines, label, bound)
    cost(table)
    busy = sorted({f"{e} {v}" for (e, v), c in table.items()
                   if any(x["foreign"] > FOREIGN_NOTE for x in c["win"].values())})
    if busy:
        print(f"\n# note: other processes used more than {FOREIGN_NOTE:.0%} of the box during "
              + ", ".join(busy) + " — if those cells matter, run arena again")
    if all(v in bound for v in VERBS):
        print("\n**No cell was set by the engines: every one is CLIENT-BOUND.**")
        return 1
    if weak:
        print("\n**Not separated from 1 at 99%:** " + ", ".join(weak) + ".")
        return 1
    print("\n**Every cell that is not CLIENT-BOUND: kevy's lower bound is above 1 against every competitor.**")
    return 0


if __name__ == "__main__":
    sys.exit(main())
