#!/usr/bin/env python3
"""Print what a bench/repack-tail.sh results directory says.

    python3 bench/repack_tail_report.py <results-dir>

1. Client latency per server and command: every round's histogram merged,
   then p50 / p99 / p99.9 / max, the spread of p99.9 across rounds, and
   how the index size moved during the load (a repack that ran shrinks
   it). A run whose box had more than 25% of a core busy with something
   else is flagged.
2. Per trace run: the step and tick time distributions; the share of slow
   steps among steps that freed a leaf, took a page fault, or lost the
   CPU; and what the slow steps have in common. A slow step whose thread
   CPU time is well under its wall time was off the CPU, and the switch
   counters say whether the scheduler took it (involuntary switch) or
   something ran on the core without a switch (an interrupt, softirq, or
   the hypervisor). One whose CPU time is its wall time spent it on the
   CPU, in the pack or in the kernel on its behalf (faults, frees).
3. The repack with no server: its step histogram and slow-step classes.
"""

import json
import pathlib
import re
import sys
from collections import defaultdict

FOREIGN_MAX = 25


def quantile(hist, q):
    n = sum(hist.values())
    if n == 0:
        return None
    want, seen = max(1, int(-(-n * q // 1))), 0
    for floor in sorted(hist):
        seen += hist[floor]
        if seen >= want:
            return floor
    return None


def us(ns):
    return "—" if ns is None else f"{ns / 1000:.1f}"


def client_section(out, runs):
    by = defaultdict(lambda: defaultdict(lambda: {"h": defaultdict(int), "max": 0, "p999": []}))
    meta = defaultdict(list)
    for path in sorted(out.glob("r*-*.json")):
        m = re.match(r"r(\d+)-(.+)\.json", path.name)
        if not m:
            continue
        server, d = m.group(2), json.loads(path.read_text())
        load = (out / (path.stem + ".load")).read_text() if (out / (path.stem + ".load")).exists() else ""
        f = re.search(r"foreign_cpu_pct=(-?\d+)", load)
        foreign = int(f.group(1)) if f else None
        meta[server].append((int(m.group(1)), d["index_before"], d["index_after"], d["errors"], foreign))
        for cmd, c in d["cmds"].items():
            cell = by[server][cmd]
            for floor, count in c["hist"]:
                cell["h"][floor] += count
            cell["max"] = max(cell["max"], int(c["max_us"] * 1000))
            cell["p999"].append(c["p999_us"])
        runs.append(server)
    if not by:
        print("no client runs found")
        return
    print("== client latency, µs (from when each request was due; all rounds merged) ==")
    print(f"{'server':<9} {'cmd':<6} {'n':>9} {'p50':>7} {'p99':>7} {'p99.9':>8} {'max':>9}   p99.9 per round")
    for server in by:
        for cmd in ("write", "read", "query"):
            c = by[server].get(cmd)
            if not c:
                continue
            h = c["h"]
            rounds = " ".join(f"{v:.0f}" for v in c["p999"])
            print(f"{server:<9} {cmd:<6} {sum(h.values()):>9} {us(quantile(h, .5)):>7} {us(quantile(h, .99)):>7} "
                  f"{us(quantile(h, .999)):>8} {us(c['max']):>9}   {rounds}")
    print()
    print("== index during the load window, and the box ==")
    for server, rows in meta.items():
        for rnd, before, after, errors, foreign in sorted(rows):
            flag = ""
            if foreign is not None and foreign > FOREIGN_MAX:
                flag = f"  ! foreign CPU {foreign}% of a core: this run measured the neighbours too"
            err = f"  ! {errors} error replies" if errors else ""
            fc = "n/a" if foreign is None else f"{foreign}%"
            print(f"  r{rnd} {server:<9} {before}  ->  {after}   foreign={fc}{flag}{err}")
    print()


def parse_trace(path):
    steps = defaultdict(lambda: defaultdict(int))  # class -> floor -> count
    ticks = defaultdict(int)
    longs, total, head = [], 0, ""
    for line in path.read_text().splitlines():
        parts = line.split()
        if not parts:
            continue
        if parts[0] == "trace":
            head = line
        elif parts[0] == "hist":
            target = ticks if parts[2] == "tick" else steps[int(parts[3])]
            for cell in parts[4:]:
                floor, count = cell.split(":")
                target[int(floor)] += int(count)
        elif parts[0] == "long_total":
            total = int(parts[1])
        elif parts[0] == "long":
            longs.append(dict(kv.split("=", 1) for kv in parts[1:]))
    return head, steps, ticks, longs, total


def num(v):
    return int(v.split("->")[-1]) if "->" in v else int(v)


def trace_section(out):
    for path in sorted(out.glob("trace-*.trace")):
        head, steps, ticks, longs, total = parse_trace(path)
        all_steps = defaultdict(int)
        for h in steps.values():
            for f, c in h.items():
                all_steps[f] += c
        long_ns = int(re.search(r"long_ns=(\d+)", head).group(1)) if "long_ns=" in head else 100_000
        print(f"== {path.stem}: {head} ==")
        n = sum(all_steps.values())
        print(f"  steps {n}: p50 {us(quantile(all_steps, .5))}  p99 {us(quantile(all_steps, .99))}  "
              f"p99.9 {us(quantile(all_steps, .999))}  max-bucket {us(max(all_steps) if all_steps else None)} µs")
        busy = {f: c for f, c in ticks.items() if f >= 50_000}
        print(f"  ticks {sum(ticks.values())} ({sum(busy.values())} of them ≥ 50 µs, i.e. packing): "
              f"p50 {us(quantile(busy, .5))}  p99 {us(quantile(busy, .99))}  max-bucket {us(max(ticks) if ticks else None)} µs")
        print(f"  slow = ≥ {long_ns // 1000} µs. share of slow steps by class (freed a leaf / faulted / lost the CPU):")
        for cls in sorted(steps):
            h = steps[cls]
            k, slow = sum(h.values()), sum(c for f, c in h.items() if f >= long_ns)
            yn = lambda bit: "y" if cls & bit else "-"
            print(f"    {yn(1)} {yn(2)} {yn(4)}   steps {k:>9}   slow {slow:>6}  ({100 * slow / max(k, 1):.3f}%)")
        explain_longs(longs, total)
        print()


def explain_longs(longs, total):
    if not longs:
        print(f"  no slow steps kept (counted: {total})")
        return
    k = len(longs)
    share = lambda pred: f"{sum(1 for r in longs if pred(r))}/{k}"
    off = lambda r: int(r["cpu_ns"]) * 2 < int(r["wall_ns"])
    print(f"  slow steps kept {k} of {total}:")
    print(f"    CPU time under half the wall time (off the CPU): {share(off)}")
    print(f"      … with an involuntary switch: {share(lambda r: off(r) and int(r['nivcsw']) > 0)}"
          f"; a voluntary one: {share(lambda r: off(r) and int(r['nvcsw']) > 0)}"
          f"; no switch (interrupt / softirq / steal): "
          f"{share(lambda r: off(r) and int(r['nivcsw']) == 0 and int(r['nvcsw']) == 0)}")
    print(f"    on the CPU for most of it: {share(lambda r: not off(r))}"
          f"; of those with page faults: {share(lambda r: not off(r) and int(r['minflt']) + int(r['majflt']) > 0)}")
    print(f"    freed a leaf: {share(lambda r: int(r['leaves_freed']) > 0)}"
          f"; freed an inner node: {share(lambda r: int(r['inners_freed']) > 0)}"
          f"; changed height: {share(lambda r: r['height'].split('->')[0] != r['height'].split('->')[1])}")
    print(f"    grew the freed-leaf list: {share(lambda r: r['free_list_cap'].split('->')[0] != r['free_list_cap'].split('->')[1])}"
          f"; grew the hand buffer: {share(lambda r: r['hand_cap'].split('->')[0] != r['hand_cap'].split('->')[1])}"
          f"; first step of its tick: {share(lambda r: int(r['nth_in_tick']) == 0)}")
    print("    slowest ten:")
    for r in sorted(longs, key=lambda r: -int(r["wall_ns"]))[:10]:
        print(f"      wall {int(r['wall_ns']) // 1000:>6} µs  cpu {int(r['cpu_ns']) // 1000:>5} µs  "
              f"minflt {r['minflt']:>3}  nvcsw {r['nvcsw']}  nivcsw {r['nivcsw']}  freed {r['leaves_freed']}  "
              f"moved {r['moved']:>4}  leaves {r['leaves']}  step {r['nth_in_tick']} of its tick")


def lib_section(out):
    path = out / "lib.txt"
    if path.exists():
        print("== the repack with no server (examples/tidy_steps.rs) ==")
        lines = path.read_text().splitlines()
        for line in lines:
            if not line.startswith("  step "):
                print("  " + line)
        slow = [l for l in lines if l.startswith("  step ")]
        print(f"  ({len(slow)} slow steps listed in lib.txt)")


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    out = pathlib.Path(sys.argv[1])
    meta = out / "meta.txt"
    if meta.exists():
        print(meta.read_text().strip())
        print()
    runs = []
    client_section(out, runs)
    trace_section(out)
    lib_section(out)
    if not runs:
        print("no measurement found in", out)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
