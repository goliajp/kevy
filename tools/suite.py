#!/usr/bin/env python3
"""The kevy test suite runner — four tiers, one manifest, no dark areas.

    python3 tools/suite.py precommit            run a tier
    python3 tools/suite.py premerge             everything CI checks on a push
    python3 tools/suite.py prerelease --list    show what a tier would run
    python3 tools/suite.py --audit              verify the manifest's invariants
    python3 tools/suite.py full --rerun-unchanged   also run rows whose inputs
                                                did not change since they passed

The manifest (suite/manifest.toml) is the single source of truth for
what is checked; this runner is deliberately dumb about content and
strict about accounting:

- A missing requirement (box, docker, a browser…) is a loud NOT-RUN row in
  the verdict, never a silent pass. "full minus these" is said out loud.
- A check that cannot be found fails the AUDIT — a deleted gate cannot
  quietly leave the suite. Every one of this repository's worst greens
  was a check that had stopped looking at anything.
- Tier cost is arithmetic, not hope: declared expected-durations are
  audited against the tier budgets, and every run records real
  durations to target/suite-<tier>.json so the declarations can be
  corrected from measurements.

Exit code: 1 on any hard FAIL or audit violation; 0 otherwise (the
verdict still lists NOT-RUN and advisory rows by name).
"""

import os
import functools
import json
import pathlib
import subprocess
import sys
import time
import tomllib

import ci_carry
import suite_requirements as req
import suite_unchanged
from suite_requirements import PROBES, children_cpu, requirement_gap

# Line-buffered even when redirected: a tier run under nohup showed a
# zero-byte log for its whole first hour, which reads as "hung" and is
# merely buffered.
print = functools.partial(print, flush=True)

ROOT = pathlib.Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "suite/manifest.toml"

TIERS = ["precommit", "premerge", "prerelease", "full"]
AREAS = {
    "hygiene", "release-pins", "arch", "doc", "perf", "mem", "disk",
    "compat", "dialect", "feature", "case", "doors", "cov",
}


def load():
    m = tomllib.loads(MANIFEST.read_text(encoding="utf-8"))
    return m["suite"], m["check"]


def tier_checks(checks, tier):
    """Inheritance: a check runs in its declared tier and above."""
    rank = {t: i for i, t in enumerate(TIERS)}
    return [c for c in checks if rank[c["tier"]] <= rank[tier]]


# ── audit ────────────────────────────────────────────────────────────

def audit(suite, checks):
    bad = []
    if len(checks) < suite["manifest_floor"]:
        bad.append(f"only {len(checks)} checks — below the manifest floor "
                   f"of {suite['manifest_floor']}; the manifest is broken")

    ids = [c["id"] for c in checks]
    for dup in {i for i in ids if ids.count(i) > 1}:
        bad.append(f"check id {dup!r} appears more than once")

    for c in checks:
        if c["tier"] not in TIERS:
            bad.append(f"{c['id']}: unknown tier {c['tier']!r}")
        if c["area"] not in AREAS:
            bad.append(f"{c['id']}: unknown area {c['area']!r}")
        # Every path-looking token in the command must exist: a renamed
        # or deleted gate must fail here, not vanish from coverage.
        # shlex, not str.split: a compound command quotes its inner
        # script, and a naive split hands back tokens wearing quote
        # marks that no filesystem contains.
        import shlex
        try:
            toks = shlex.split(c["cmd"])
        except ValueError:
            toks = c["cmd"].split()
        inner = []
        for t in toks:
            inner += shlex.split(t) if (" " in t) else [t]
        for tok in inner:
            if "=" in tok and tok.split("=", 1)[0].isidentifier():
                tok = tok.split("=", 1)[1]  # an env assignment's value
            if "/" in tok and not tok.startswith("-") and not (ROOT / tok).exists():
                if tok.startswith("target/"):
                    continue  # build products are a requirement, not a file check
                if any(ch in tok for ch in "$()\""):
                    continue  # a substitution, not a path
                bad.append(f"{c['id']}: {tok} does not exist")
        if c.get("expected", 0) > c.get("timeout", 0):
            bad.append(f"{c['id']}: expected {c['expected']}s exceeds its own timeout")

    # Budgets are arithmetic: the declared expected-durations of a tier
    # must fit its budget, and the tiers must order strictly.
    budgets = suite["budgets"]
    if not budgets["precommit"] < budgets["premerge"] < budgets["prerelease"]:
        bad.append("budget order violated: precommit < premerge < prerelease")
    for tier in ("precommit", "premerge", "prerelease"):
        total = sum(c["expected"] for c in tier_checks(checks, tier)
                    if not requirement_needs_infra(c))
        if total > budgets[tier]:
            bad.append(f"{tier}: declared durations sum to {total}s, over the "
                       f"{budgets[tier]}s budget — the tier stopped being what it claims")

    # Every requirement must say where it is met, and every declaration must
    # be for a requirement something asks for. Without this the manifest can
    # name a requirement nothing satisfies and it is indistinguishable from
    # one satisfied on a machine nobody is at — the NOT-RUN line says what is
    # missing, never where the row does run.
    # `advisory` means the row can never redden its tier, while its `proves`
    # line goes on claiming something. This project already requires a reason
    # attached to a waiver and to an accepted set-growth; the same rule, for
    # the same reason: the exemption must not outlive whoever understood it.
    for c in checks:
        if c.get("advisory") and not c.get("advisory_reason", "").strip():
            bad.append(f"{c['id']}: advisory = true with no advisory_reason — "
                       f"a row that cannot fail must say why, and what would "
                       f"end that")
        if c.get("advisory_reason", "").strip() and not c.get("advisory"):
            bad.append(f"{c['id']}: has an advisory_reason but is not advisory "
                       f"— the reason outlived the exemption")

    declared = suite.get("requirements", {})
    used = {r for c in checks for r in c.get("requires", [])}
    for r in sorted(used - set(declared)):
        bad.append(f"requirement {r!r} is named by a check but not declared in "
                   f"[suite.requirements] — nothing says where it is met")
    for r in sorted(used - set(PROBES)):
        bad.append(f"requirement {r!r} has no probe in tools/suite.py — a tier "
                   f"that reaches a check asking for it stops with a KeyError")
    for r in sorted(set(declared) - used):
        bad.append(f"requirement {r!r} is declared but no check asks for it — "
                   f"the list rotted")

    for cid, pat in suite_unchanged.dead_patterns(ROOT, checks):
        bad.append(f"{cid}: input {pat!r} matches no tracked file — the row "
                   f"would count as unchanged forever")

    # No dark areas in full.
    covered = {c["area"] for c in checks}
    for area in sorted(AREAS - covered):
        bad.append(f"area {area!r} has no check at all — a dark corner")

    nowhere = sorted(r for r, where in suite.get("requirements", {}).items()
                     if not where.strip())
    if nowhere:
        by_req = {r: [c["id"] for c in checks if r in c.get("requires", [])]
                  for r in nowhere}
        print("suite audit: NOTE — requirement(s) declared as met nowhere:")
        for r, who in by_req.items():
            print(f"    ⊘ {r!r} — {', '.join(who)} runs in no environment this "
                  f"project has")

    if bad:
        print(f"suite audit: FAIL — {len(bad)} problem(s)")
        for b in bad:
            print(f"  ✗ {b}")
        return 1
    n = {t: len(tier_checks(checks, t)) for t in TIERS}
    print(f"suite audit: ok — {len(checks)} checks "
          f"(precommit {n['precommit']} ⊆ premerge {n['premerge']} ⊆ prerelease {n['prerelease']} "
          f"⊆ full {n['full']}), "
          f"{len(covered)} areas covered, budgets hold")
    return 0


def requirement_needs_infra(check):
    """Checks whose requirements are inherently absent on some hosts do
    not count against the local budget arithmetic."""
    return "box" in check.get("requires", [])


# ── run ──────────────────────────────────────────────────────────────

def keep_log(tier, check_id, text):
    """A row's whole output. A failure's verdict shows six lines, and a
    flake's cause is rarely in the last six; rerunning to see it again is
    how the evidence of an intermittent failure gets lost. A pass shows
    none, and a measurement gate's numbers are the result."""
    path = ROOT / "target" / "suite-logs" / f"{tier}-{check_id}.log"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path.relative_to(ROOT)


def run_tier(suite, checks, tier, only=None, area=None, rerun=False):
    selected = tier_checks(checks, tier)
    if only:
        selected = [c for c in selected if c["id"] == only]
        if not selected:
            sys.exit(f"suite: no check named {only!r} in tier {tier}")
    if area:
        selected = [c for c in selected if c["area"] == area]

    results, cpu_of = [], {}
    t_start = time.monotonic()
    req.RUN_STARTED = time.time()
    carry, carry_why = ({}, "") if only or area else ci_carry.carried(ROOT, selected, tier)
    if carry_why:
        print(f"  ci-carry: {carry_why}")
    same = {} if only or area or rerun or tier in ("precommit", "premerge") \
        else suite_unchanged.unchanged(ROOT, selected)
    for c in selected:
        if c["id"] in carry:
            results.append((c, "CARRIED", 0.0, carry[c["id"]], False))
            print(f"  ↺ {c['id']:<22} CARRIED  ({carry[c['id']]})")
            continue
        if c["id"] in same:
            results.append((c, "UNCHANGED", 0.0, same[c["id"]], False))
            print(f"  = {c['id']:<22} UNCHANGED  ({same[c['id']]})")
            continue
        gap = requirement_gap(c)
        if gap:
            results.append((c, "NOT-RUN", 0.0, gap, False))
            print(f"  ⊘ {c['id']:<22} NOT-RUN  ({gap})")
            continue
        t0, cpu0 = time.monotonic(), children_cpu()
        try:
            # Its own process group, so a timeout kills the whole tree.
            # The first timeout this runner ever fired killed the check's
            # shell and orphaned the servers the check had started — which
            # went on writing runtime files into the repo root, and the
            # NEXT check (rootgate) failed for it. A kill that leaves the
            # children alive converts one red into two, a run apart.
            import signal
            proc = subprocess.Popen(
                c["cmd"], shell=True, cwd=ROOT,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True,
            )
            try:
                out, err = proc.communicate(timeout=c["timeout"])
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                out, err = proc.communicate()
                keep_log(tier, c["id"], (out or "") + (err or ""))
                raise
            r = subprocess.CompletedProcess(c["cmd"], proc.returncode, out, err)
            took = time.monotonic() - t0
            cpu_of[c["id"]] = children_cpu() - cpu0
            if r.returncode == 0:
                results.append((c, "PASS", took, "", True))
                # a passing measurement gate's numbers are its result
                keep_log(tier, c["id"], r.stdout + r.stderr)
                print(f"  ✓ {c['id']:<22} {took:6.1f}s")
            else:
                tail = (r.stdout + r.stderr).strip().splitlines()[-6:]
                status = "ADVISORY" if c.get("advisory") else "FAIL"
                results.append((c, status, took, "\n".join(tail), True))
                mark = "△" if status == "ADVISORY" else "✗"
                log = keep_log(tier, c["id"], r.stdout + r.stderr)
                print(f"  {mark} {c['id']:<22} {took:6.1f}s  {status}  (whole output: {log})")
                for line in tail:
                    print(f"      {line[:140]}")
        except subprocess.TimeoutExpired:
            took = time.monotonic() - t0
            cpu_of[c["id"]] = children_cpu() - cpu0
            results.append((c, "TIMEOUT", took, f"timed out after {c['timeout']}s", False))
            print(f"  ✗ {c['id']:<22} {took:6.1f}s  TIMEOUT ({c['timeout']}s)  "
                  f"(output so far: target/suite-logs/{tier}-{c['id']}.log)")

    # Exit hygiene: the tier leaves the tree as it found it. rootgate
    # runs first as a check, but residue produced BY the tier lands
    # after it looked — check_doc_toml did exactly that for months, and
    # the red was billed to whoever ran next. Not a manifest entry so it
    # cannot be reordered or forgotten.
    if not only and not area:
        # Leaked servers first: a gate that exits without killing what it
        # spawned leaves a squatter that makes a LATER gate refuse — the
        # box's first full run had capacity-envelope refuse over a server
        # some earlier check had leaked. Only processes running THIS
        # repo's binaries are ours to kill; another session's servers are
        # not, and pgrep's own invocation must not match itself.
        import signal as sig
        leaked = subprocess.run(
            ["pgrep", "-af", str(ROOT / "target")],
            capture_output=True, text=True).stdout.strip()
        leaked_rows = [l for l in leaked.splitlines()
                       if "/kevy" in l and "pgrep" not in l]
        if leaked_rows:
            print(f"  ✗ exit-hygiene: {len(leaked_rows)} leaked server(s), killed:")
            for row in leaked_rows:
                print(f"      {row[:130]}")
                try:
                    os.kill(int(row.split()[0]), sig.SIGKILL)
                except (ValueError, ProcessLookupError, PermissionError):
                    pass
            results.append(({"id": "exit-hygiene-procs", "area": "hygiene"},
                            "FAIL", 0.0, "\n".join(leaked_rows[:4]), False))
        sweep = subprocess.run(["bash", "bench/rootgate.sh"], cwd=ROOT,
                               capture_output=True, text=True)
        if sweep.returncode != 0:
            tail = sweep.stdout.strip().splitlines()[:4]
            results.append(({"id": "exit-hygiene", "area": "hygiene"},
                            "FAIL", 0.0, "\n".join(tail), False))
            print(f"  ✗ exit-hygiene: the tier itself left residue behind")
            for line in tail:
                print(f"      {line[:140]}")

    wall = time.monotonic() - t_start
    fails = [r for r in results if r[1] == "FAIL"]
    notrun = [r for r in results if r[1] == "NOT-RUN"]
    advis = [r for r in results if r[1] == "ADVISORY"]
    passed = [r for r in results if r[1] == "PASS"]
    timeouts = [r for r in results if r[1] == "TIMEOUT"]
    carried_rows = [r for r in results if r[1] == "CARRIED"]
    same_rows = [r for r in results if r[1] == "UNCHANGED"]
    suite_unchanged.record(ROOT, results)

    # Real durations land beside the build products so the declared
    # expectations can be corrected from measurement, and cleaning the
    # build cleans this too.
    # Only a whole tier writes the tier's ledger. `--only` and `--area` run a
    # subset, and a subset that overwrites the file destroys the record it is
    # not a substitute for: three of these files were one row each by the time
    # anyone looked, and suite-full.json's single `mcpgate` row — read for a
    # while as "one row in the old format" — was a `--only mcpgate` standing
    # where 99 measurements had been.
    out = ROOT / f"target/suite-{tier}.json"
    out.parent.mkdir(exist_ok=True)
    # `seconds` is a measurement only where `measured` says so, and each
    # row decides that where it is appended rather than here, because the
    # answer does not follow from the status alone. A TIMEOUT row's seconds
    # is the ceiling it hit, and recording the two in one field is how
    # 120.1 s of timeout became "this gate costs two minutes" in a later
    # decomposition. A NOT-RUN row never ran, and the two FAIL rows this
    # runner synthesises after the tier carry no duration at all. None of
    # those is what the check costs, and all used to be filed as though
    # they were.
    if not only and not area:
        out.write_text(json.dumps(
            [{"id": c["id"], "status": s, "seconds": round(t, 1),
              "cpu_seconds": round(cpu_of.get(c["id"], 0.0), 1),
              "measured": m,
              "ceiling": c["timeout"] if s == "TIMEOUT" else None} for c, s, t, _, m in results],
            indent=1))

    budget = suite["budgets"].get(tier)
    print(f"\nsuite {tier}: {len(passed)} passed, {len(fails)} failed, "
          f"{len(timeouts)} timed out, {len(advis)} advisory, "
          f"{len(notrun)} not-run, {len(carried_rows)} carried from CI, "
          f"{len(same_rows)} unchanged since they passed here — "
          f"{wall:.0f}s" + (f" (budget {budget}s)" if budget else ""))
    # The tally must account for every check that was selected. It did not:
    # a TIMEOUT was in neither the counts nor the failed list,
    # so `workspace-tests` hit its 5400s ceiling and 53 checks were reported
    # as "43 passed, 2 failed, 1 advisory, 5 not-run". Eleven short of the
    # truth, in a line whose whole job is to be the truth.
    counted = (len(passed) + len(fails) + len(timeouts) + len(advis)
               + len(notrun) + len(carried_rows) + len(same_rows))
    if counted != len(results):
        print(f"  ✗ the tally covers {counted} of {len(results)} checks — a status this "
              f"runner does not count is a check that disappeared from its own report")
        return 1
    if notrun:
        print("  not run here (loudly, not silently):")
        for c, _, _, why, _ in notrun:
            reqs = suite.get("requirements", {})
            where = [reqs.get(r, "") for r in c.get("requires", [])]
            where = [w for w in where if w]
            tail = f"  — runs in: {'; '.join(sorted(set(where)))}" if where else \
                   "  — runs in NO environment this project has"
            print(f"    ⊘ {c['id']}: {why}{tail}")
    if advis:
        for c, _, _, why, _ in advis:
            print(f"  △ advisory {c['id']}: {why.splitlines()[-1][:120] if why else ''}")
            # Why it cannot redden the tier, at the moment it did not.
            reason = " ".join(c.get("advisory_reason", "").split())
            if reason:
                print(f"      advisory because: {reason[:180]}")
    if fails or timeouts:
        print("  failed:")
        for c, _, _, _, _ in fails:
            print(f"    ✗ {c['id']}")
        # A check that hit its ceiling did not pass, and did not report a
        # verdict either. Counting it as neither is how one vanished.
        for c, _, _, _, _ in timeouts:
            print(f"    ✗ {c['id']} — timed out at {c['timeout']}s, never finished")
        return 1
    if budget and wall > budget:
        print(f"  ✗ the tier ran over its own budget ({wall:.0f}s > {budget}s) — "
              f"that is a failure of the tier's promise, not of any check")
        return 1
    return 0


def main():
    args = sys.argv[1:]
    suite, checks = load()
    if "--audit" in args:
        return audit(suite, checks)
    tier = next((a for a in args if a in TIERS), None)
    if tier is None:
        print(__doc__)
        return 2
    if "--list" in args:
        for c in tier_checks(checks, tier):
            req = ",".join(c.get("requires", [])) or "-"
            print(f"  {c['id']:<22} {c['area']:<12} ~{c['expected']:>5}s  [{req}]  {c['proves']}")
        return 0
    only = next((args[i + 1] for i, a in enumerate(args) if a == "--only"), None)
    area = next((args[i + 1] for i, a in enumerate(args) if a == "--area"), None)
    # The audit runs before every tier: a run against a broken manifest
    # would report coverage the manifest no longer has.
    if audit(suite, checks) != 0:
        return 1
    print()
    return run_tier(suite, checks, tier, only=only, area=area,
                    rerun="--rerun-unchanged" in args)


if __name__ == "__main__":
    sys.exit(main())
