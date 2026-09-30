"""perfgate's output: the header, the ratio table, the absolute table, verdicts."""

import pathlib
import statistics
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "tools"))
import perfreport as pr  # noqa: E402

RATIO_COLS = ("ops", "instr", "cycles", "sys", "p95_us")
GATED = ("ops", "instr", "cycles", "p95_us")
UTIL_MIN = 0.90
CLIENT_MAX = 0.90
FOREIGN_MAX = 0.05


def header(mode, sides, topo, rounds, windows, secs):
    print(f"# perfgate {mode} — {time.strftime('%Y-%m-%d %H:%M %Z')} — box {topo['box']}")
    for name, s in zip("AB", sides):
        env = f" — env {s['env_note']}" if s["env_note"] else ""
        print(f"# {name}: {s['label']} — {s['version']} — {s['bin']}{env}")
    print(f"# server cpus {topo['srv_cpus']} ({topo['srv_threads']} threads), load cpus "
          f"{topo['cli_cpus']} ({topo['cli_threads']} threads); {rounds} rounds, "
          f"{windows} x {secs} s windows per side, the side that goes first alternates")
    sys.stdout.flush()


def medians(obs, key):
    vals = [o.get(key) for o in obs if o.get(key) is not None]
    return statistics.median(vals) if vals else None


def angle_verdict(pairs, cfg):
    """('ok'|'FAIL'|'NOISY', [metrics that decided it])"""
    by = {}
    for m in GATED:
        rs = pr.ratios(pairs, m)
        v = pr.verdict(m, rs, cfg["limits"][m], cfg["noise"][m])
        by.setdefault(v, []).append(m)
    for v in ("FAIL", "NOISY"):
        if v in by:
            return v, by[v]
    return "ok", []


def ratio_table(results, cfg):
    cols = [pr.METRICS[c][0] for c in RATIO_COLS]
    print("\n## B / A per round: median ±half-range\n")
    print(f"{'angle':<14}" + "".join(f"{c:>15}" for c in cols)
          + f"{'util A|B':>12}{'fgn%':>6}  verdict")
    worst = "ok"
    for angle, pairs in results.items():
        a, b = [p[0] for p in pairs], [p[1] for p in pairs]
        cells = [pr.fmt_ratio(pr.summarize(pairs, c)) for c in RATIO_COLS]
        ua, ub = medians(a, "util"), medians(b, "util")
        util = "—" if ua is None or ub is None else f"{ua:.2f}|{ub:.2f}"
        fgn = max((o.get("foreign") or 0.0) for o in a + b)
        v, why = angle_verdict(pairs, cfg)
        worst = v if v == "FAIL" or (v == "NOISY" and worst == "ok") else worst
        print(f"{angle:<14}" + "".join(f"{c:>15}" for c in cells)
              + f"{util:>12}{fgn * 100:>6.1f}  {v}{' ' + ','.join(why) if why else ''}")
    return worst


def absolute_table(results):
    print("\n## medians per side\n")
    print(f"{'angle':<14}{'side':>5}{'ops/s':>10}{'instr/op':>10}{'user':>8}{'kernel':>8}"
          f"{'cyc/op':>9}{'sys/op':>8}{'p95 µs':>8}{'rss MB':>8}")
    for angle, pairs in results.items():
        for s, name in ((0, "A"), (1, "B")):
            obs = [p[s] for p in pairs]
            m = {k: medians(obs, k) for k in ("ops", "instr", "instr_u", "instr_k", "cycles",
                                              "sys", "p95_us", "rss")}
            print(f"{angle if s == 0 else '':<14}{name:>5}{pr.fmt_abs(m['ops'], 'ops'):>10}"
                  f"{pr.fmt_abs(m['instr'], 'instr'):>10}{pr.fmt_abs(m['instr_u'], 'instr'):>8}"
                  f"{pr.fmt_abs(m['instr_k'], 'instr'):>8}{pr.fmt_abs(m['cycles'], 'cycles'):>9}"
                  f"{pr.fmt_abs(m['sys'], 'sys'):>8}{pr.fmt_abs(m['p95_us'], 'p95_us'):>8}"
                  f"{pr.fmt_abs(m['rss'], 'rss'):>8}")


def notes(results):
    out = []
    for angle, pairs in results.items():
        for s, name in ((0, "A"), (1, "B")):
            obs = [p[s] for p in pairs]
            u, c = medians(obs, "util"), max((o.get("client") or 0.0) for o in obs)
            if u is not None and u < UTIL_MIN:
                out.append(f"{angle} {name}: server utilisation {u:.2f} — not saturated, so "
                           "instr/op and cyc/op include idle work")
            if c > CLIENT_MAX:
                out.append(f"{angle} {name}: load generators at {c:.0%} of their cpus — "
                           "the throughput may be the client's")
        fgn = max((o.get("foreign") or 0.0) for p in pairs for o in p)
        if fgn > FOREIGN_MAX:
            out.append(f"{angle}: up to {fgn:.0%} of the box was used by other processes — "
                       "if the verdict matters, run it again")
    return out


def report(mode, results, cfg):
    worst = ratio_table(results, cfg)
    absolute_table(results)
    lim = ", ".join(f"{pr.METRICS[m][0]} {'≥' if pr.METRICS[m][1] == 'rate' else '≤'} "
                    f"{cfg['limits'][m]:.2f} (noise {cfg['noise'][m] * 100:.1f}%)" for m in GATED)
    print(f"\n# limits on B / A: {lim}")
    for n in notes(results):
        print(f"# note: {n}")
    if mode != "gate":
        return 0
    code = {"ok": 0, "FAIL": 1, "NOISY": 3}[worst]
    print({0: "perfgate: PASS",
           1: "perfgate: FAIL — B is beyond a limit on every round, or on the median of "
              "rounds that agree",
           3: "perfgate: NOISY — rounds disagree across a limit; run it again"}[code])
    return code
