#!/usr/bin/env python3
"""perfgate2's bookkeeping: pairs, verdicts, calibration, the ledger.

  perfgate2_judge.py check     --baseline B --fingerprint FP --angles "..." [--lines C|T|all]
  perfgate2_judge.py next      --samples S --baseline B --fingerprint FP --angle A [--lines ...]
  perfgate2_judge.py spin      --samples S --angle A --frac F
  perfgate2_judge.py report    --samples S --baseline B --fingerprint FP --angles "..." --ref R [--lines ...] [--mutant M]
  perfgate2_judge.py calibrate --samples S --baseline B --fingerprint FP --angles "..."
  perfgate2_judge.py ledger    --samples S --out L --fingerprint FP --commit C --ref R
  perfgate2_judge.py fingerprint --baseline B          (lscpu -e on stdin)
  perfgate2_judge.py advance   --baseline B --sha S --anchor T

S is the JSONL of kept windows written by perfgate2_window.py.
Exit codes of report: 0 pass, 1 regression (or unproven mutant), 3 undecided;
every subcommand exits 2 on a refusal.
"""

import argparse
import collections
import datetime
import json
import math
import pathlib
import statistics
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "tools"))
import perfstats as ps  # noqa: E402
import perfwindow as pw  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
# instructions per iteration of the M1 mutant's loop (dec + jnz)
SPIN_INSTR_PER_ITER = 2


def load_windows(path):
    p = pathlib.Path(path)
    if not p.exists():
        return []
    return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]


def observations(windows, topo, angle):
    """{side: {obs: observation}} for one angle."""
    by = collections.defaultdict(list)
    for w in windows:
        if w["angle"] == angle:
            by[(w["side"], w["obs"])].append(w)
    out = {"ref": {}, "cand": {}}
    for (side, obs), ws in by.items():
        out[side][obs] = pw.observation(ws, topo)
    return out


def angle_lines(angle, mode):
    if angle.endswith("_us"):
        return [] if mode == "C" else ["L.us"]
    if mode == "C":
        return list(ps.C_LINES)
    if mode == "T":
        return ["T.ops"]
    return ["C.instr_u", "C.instr_k", "C.sys", "S.cyc", "S.util", "T.ops"]


def pair_ds(obs, lines):
    """{line: [d...]} over complete pairs in pair order."""
    ds = {line: [] for line in lines}
    for k in sorted(set(obs["ref"]) & set(obs["cand"])):
        ref, cand = obs["ref"][k], obs["cand"][k]
        for line in pw.pair_lines(ref, cand, lines):
            ds[line].append(ps.pair_d(line, ref["lines"][line], cand["lines"][line]))
    return ds


def sigmas(baseline, angle, lines, fp):
    today = datetime.date.today()
    return {line: ps.sigma_for(baseline, angle, line, fp, today) for line in lines}


def judge_angle(baseline, obs, angle, lines, fp):
    p = baseline["sprt"]
    sig = sigmas(baseline, angle, lines, fp)
    ds = pair_ds(obs, lines)
    out = {}
    for line in lines:
        if sig[line] is None:
            out[line] = {"state": "not-judged", "ds": ds[line], "why": "calibration could not measure it"}
            continue
        r = ps.judge_line(ds[line], ps.delta_for(line, baseline["bands"]), sig[line], p)
        r["ds"], r["sigma"] = ds[line], sig[line]
        out[line] = r
    return out


def cmd_check(a, baseline):
    for angle in a.angles.split():
        sigmas(baseline, angle, angle_lines(angle, a.lines), a.fingerprint)
    print("perfgate2: sigma present, current, and from this topology")


def cmd_next(a, baseline):
    obs = observations(load_windows(a.samples), baseline["topology"], a.angle)
    lines = angle_lines(a.angle, a.lines)
    pairs = len(set(obs["ref"]) & set(obs["cand"]))
    cap = baseline["sprt"]["n_max"] + baseline["sprt"]["confirm_pairs"]
    res = judge_angle(baseline, obs, a.angle, lines, a.fingerprint)
    more = pairs < baseline["sprt"]["n_min"] or any(ps.wants_more(r) for r in res.values())
    print("more" if more and pairs < cap else "done")


def cmd_spin(a, _baseline):
    ws = [w for w in load_windows(a.samples)
          if w["angle"] == a.angle and w["side"] == "ref" and w.get("perf")]
    if not ws:
        raise ps.Refused(f"no reference window with counters yet for {a.angle}")
    topo = {"srv_cpus": ws[0]["srv_cpus"]}
    instr = statistics.fmean(pw.window_lines(w, topo)["C.instr_u"] for w in ws)
    print(max(1, round(instr * a.frac / SPIN_INSTR_PER_ITER)))


def fmt(line, v):
    if v is None:
        return "—"
    if line == "T.ops":
        return f"{v / 1e6:.3f}M"
    if line == "S.util":
        return f"{v:.3f}"
    return f"{v:.2f}" if v < 100 else f"{v:.0f}"


def side_mean(obs, side, line):
    vals = [o["lines"][line] for o in obs[side].values() if line in o["lines"]]
    return statistics.fmean(vals) if vals else None


def report_angle(angle, obs, res, baseline):
    rows = []
    for line, r in res.items():
        n = len(r["ds"])
        verdict = ps.finalize(r, n, baseline["sprt"]["n_min"]) if "llr" in r else r["state"]
        ref, cand = side_mean(obs, "ref", line), side_mean(obs, "cand", line)
        shift = ""
        if r["ds"]:
            m = statistics.fmean(r["ds"])
            shift = f"{m * 100:+.2f}pp" if line == "S.util" else f"{math.expm1(m) * 100:+.2f}%"
        extra = ""
        if verdict == "undecided" and r.get("sigma"):
            lo, hi = ps.ci99(r["ds"], r["sigma"])
            extra = f"  99% CI of the worsening [{math.expm1(lo) * 100:+.2f}%, {math.expm1(hi) * 100:+.2f}%]"
        if r.get("confirm"):
            extra += f"  (first red confirmation: {r['confirm']['state']})"
        if r.get("why"):
            extra += f"  ({r['why']})"
        rows.append((line, verdict, f"  {angle:<24} {line:<10} {fmt(line, ref):>10} → "
                     f"{fmt(line, cand):<10} {shift:>9}  n={n:<2} {verdict}{extra}"))
    return rows


def box_line(angle, obs):
    ghz = [o["lines"]["ghz"] for s in ("ref", "cand") for o in obs[s].values() if "ghz" in o["lines"]]
    foreign = [o["foreign"] for s in ("ref", "cand") for o in obs[s].values()]
    parts = []
    if ghz:
        parts.append(f"GHz {min(ghz):.2f}-{max(ghz):.2f}")
    if foreign:
        parts.append(f"box foreign max {max(foreign):.1%}")
    return f"  {angle:<24} " + ", ".join(parts) if parts else None


def verdicts(a, baseline):
    windows = load_windows(a.samples)
    out = {}
    for angle in a.angles.split():
        lines = angle_lines(angle, a.lines)
        if not lines:
            continue
        obs = observations(windows, baseline["topology"], angle)
        out[angle] = (obs, judge_angle(baseline, obs, angle, lines, a.fingerprint))
    return out


def cmd_report(a, baseline):
    results = verdicts(a, baseline)
    print(f"perfgate2: candidate vs {a.ref} — per line: ref → cand, worsening, pairs, verdict")
    if a.lines == "T":
        print("perfgate2: no counters: this run cannot judge a 3% change")
    table = {}
    for angle, (obs, res) in results.items():
        for line, verdict, text in report_angle(angle, obs, res, baseline):
            print(text)
            table[(angle, line)] = verdict
        b = box_line(angle, obs)
        if b:
            print(b)
    if a.mutant:
        return prove(a.mutant, table)
    return overall(table)


def overall(table):
    red = sorted(k for k, v in table.items() if v == "red")
    und = sorted(k for k, v in table.items() if v == "undecided")
    t_nj = sorted(k for k, v in table.items() if v == "not-judged" and k[1] == "T.ops")
    if t_nj:
        print("perfgate2: T line NOT JUDGED on " + ", ".join(a for a, _ in t_nj)
              + " — the box was not quiet; the product number is not checked by this run")
    if red:
        print("perfgate2: FAIL — red: " + ", ".join(f"{a} {l}" for a, l in red), file=sys.stderr)
        t_only = {a for a, l in red if l == "T.ops"} - {a for a, l in red if l != "T.ops"}
        if t_only:
            print("perfgate2: throughput fell but work and cycles per op did not on "
                  + ", ".join(sorted(t_only)) + ": check the load generator, the network stack, sleeps")
        return 1
    if und:
        print("perfgate2: UNDECIDED — " + ", ".join(f"{a} {l}" for a, l in und)
              + ": add pairs (PERFGATE_ANGLES=...) or decompose", file=sys.stderr)
        return 3
    print("perfgate2: PASS")
    return 0


def prove(name, table):
    exp = json.loads((HERE / "mutants" / "expect.json").read_text())[name]
    angles = sorted({a for a, _ in table})
    hits, misses = 0, []
    for angle in angles:
        v = {l: s for (a, l), s in table.items() if a == angle}
        bad = [f"{l} is {v[l]}, must be red" for l in exp.get("red", []) if v.get(l) not in ("red", "not-judged", None)]
        bad += [f"{l} is {v[l]}, must be green" for l in exp.get("green", []) if v.get(l) not in ("green", "not-judged", None)]
        bad += [f"{l} is red, must not be" for l in exp.get("not_red", []) if v.get(l) == "red"]
        any_of = exp.get("red_any", [])
        if any_of and not any(v.get(l) == "red" for l in any_of):
            bad.append("none of " + "/".join(any_of) + " is red")
        judged = any(v.get(l) not in ("not-judged", None) for l in exp.get("red", []) + any_of + exp.get("not_red", []))
        if bad:
            misses.append(f"{angle}: " + "; ".join(bad))
        elif judged:
            hits += 1
    for m in misses:
        print(f"  ✗ {m}")
    ok = not misses and hits > 0
    print(f"perfgate2: mutant {name} {'PROVEN' if ok else 'NOT PROVEN'} on {hits}/{len(angles)} angles")
    return 0 if ok else 1


def cmd_calibrate(a, baseline):
    windows = load_windows(a.samples)
    table = {}
    for angle in a.angles.split():
        lines = angle_lines(angle, "all")
        ds = pair_ds(observations(windows, baseline["topology"], angle), lines)
        table[angle] = {line: ps.sigma_of(ds[line]) for line in lines}
        cells = "  ".join(f"{l} {'—' if s is None else f'{s:.4f}'} (n={len(ds[l])})"
                          for l, s in table[angle].items())
        print(f"  {angle:<24} {cells}")
    baseline["sigma"] = table
    baseline["sigma_recorded"] = datetime.date.today().isoformat()
    baseline["sigma_fingerprint"] = a.fingerprint
    write_baseline(a.baseline, baseline)
    print(f"perfgate2: sigma recorded -> {a.baseline} (commit it)")


def cmd_ledger(a, baseline):
    """Append-only facts: every observation's absolute line values."""
    windows = load_windows(a.samples)
    out = pathlib.Path(a.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    now = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
    n = 0
    with out.open("a") as f:
        for angle in sorted({w["angle"] for w in windows}):
            obs = observations(windows, baseline["topology"], angle)
            for side in ("ref", "cand"):
                for k, o in sorted(obs[side].items()):
                    for line, value in o["lines"].items():
                        if line == "ghz":
                            continue
                        f.write(json.dumps({
                            "time": now, "angle": angle, "line": line, "value": value,
                            "side": side, "obs": k,
                            "commit": a.commit if side == "cand" else a.ref, "ref": a.ref,
                            "ghz": o["lines"].get("ghz"), "fingerprint": a.fingerprint},
                            sort_keys=True) + "\n")
                        n += 1
    print(f"perfgate2: {n} facts appended to {out}")


def cmd_fingerprint(a, baseline):
    print(ps.fingerprint(sys.stdin.read(), baseline["topology"]))


def cmd_advance(a, baseline):
    baseline["rolling_ref"] = a.sha
    baseline["anchor_tag"] = a.anchor
    write_baseline(a.baseline, baseline)
    print(f"perfgate2: rolling reference advanced to {a.sha[:12]} (commit {a.baseline})")


def write_baseline(path, baseline):
    pathlib.Path(path).write_text(json.dumps(baseline, indent=2) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd")
    for k in ("samples", "baseline", "fingerprint", "angle", "angles", "ref",
              "mutant", "out", "commit", "sha", "anchor"):
        ap.add_argument("--" + k)
    ap.add_argument("--lines", default="all")
    ap.add_argument("--frac", type=float)
    a = ap.parse_args()
    baseline = json.loads(pathlib.Path(a.baseline).read_text()) if a.baseline else None
    fn = globals()["cmd_" + a.cmd]
    try:
        rc = fn(a, baseline)
    except ps.Refused as e:
        print(f"perfgate2: REFUSED — {e}", file=sys.stderr)
        sys.exit(2)
    sys.exit(rc or 0)


if __name__ == "__main__":
    main()
