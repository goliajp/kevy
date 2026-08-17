#!/usr/bin/env python3
"""The kevy test suite runner — three tiers, one manifest, no dark areas.

    python3 tools/suite.py precommit            run a tier
    python3 tools/suite.py prerelease --list    show what a tier would run
    python3 tools/suite.py --audit              verify the manifest's invariants

The manifest (suite/manifest.toml) is the single source of truth for
what is checked; this runner is deliberately dumb about content and
strict about accounting:

- A missing requirement (box, device, docker…) is a loud NOT-RUN row in
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

import functools
import json
import pathlib
import shutil
import subprocess
import sys
import time
import tomllib

# Line-buffered even when redirected: a tier run under nohup showed a
# zero-byte log for its whole first hour, which reads as "hung" and is
# merely buffered.
print = functools.partial(print, flush=True)

ROOT = pathlib.Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "suite/manifest.toml"

TIERS = ["precommit", "prerelease", "full"]
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


# ── requirement detection ────────────────────────────────────────────
# Each answers (available, why-not). Detection is cheap and honest:
# where we cannot know, the answer is "not here", said as such.

def _have_server(profile):
    p = ROOT / f"target/{profile}/kevy"
    if p.exists():
        return True, ""
    return False, f"target/{profile}/kevy is not built"


def _have_linux():
    import platform
    return (platform.system() == "Linux"), "not a Linux host"


def _have_box():
    import platform
    if platform.system() == "Linux" and (os_cpus() or 0) >= 16:
        return True, ""
    return False, "needs the 16-core Linux box (quiet, core-pinnable)"


def os_cpus():
    import os
    try:
        return len(os.sched_getaffinity(0))  # type: ignore[attr-defined]
    except AttributeError:
        return os.cpu_count()


def _have_node():
    if shutil.which("node"):
        return True, ""
    return False, "node is not on PATH"


def _have_chromium():
    if (ROOT / "web/node_modules/playwright-core").exists():
        return True, ""
    return False, "web/node_modules is not installed (npm ci in web/)"


def _have_web_deps():
    # Distinct from node itself: the box has node and no web/node_modules,
    # and the first box run failed four site checks that should have been
    # honest NOT-RUNs for exactly this gap.
    if (ROOT / "web/node_modules").exists():
        return True, ""
    return False, "web/node_modules is not installed (npm ci in web/)"


def _have_wasm_artifact():
    if (ROOT / "crates/kevy-wasm/pkg/kevy.wasm").exists():
        return True, ""
    return False, "crates/kevy-wasm/pkg/kevy.wasm is not built (npm run engine in web/)"


def _have_docker():
    if not shutil.which("docker"):
        return False, "docker is not on PATH"
    r = subprocess.run(["docker", "info"], capture_output=True, timeout=15)
    return (r.returncode == 0), "docker daemon is not running"


def _have_pgcmp_infra():
    import socket
    try:
        socket.create_connection(("127.0.0.1", 15499), timeout=2).close()
    except OSError:
        return False, "no Postgres on 127.0.0.1:15499 (root starts kevy-pgcmp once; see bench/pgcompare.sh)"
    venv = pathlib.Path.home() / "pgbench-venv/bin/python"
    if not venv.exists():
        return False, "no psycopg venv at ~/pgbench-venv"
    return True, ""


def _have_device():
    import os
    if os.environ.get("KEVY_DEVICE") == "1":
        return True, ""
    return False, "no device session (set KEVY_DEVICE=1 on the machine that has one)"


def requirement_gap(check):
    """The first unmet requirement, or None."""
    for r in check.get("requires", []):
        ok, why = {
            "server-debug": lambda: _have_server("debug"),
            "server-release": lambda: _have_server("release"),
            "linux": _have_linux,
            "box": _have_box,
            "node": _have_node,
            "chromium": _have_chromium,
            "docker": _have_docker,
            "web-deps": _have_web_deps,
            "pgcmp-infra": _have_pgcmp_infra,
            "wasm-artifact": _have_wasm_artifact,
            "device": _have_device,
            "ci": lambda: (False, "runs in CI, not locally"),
        }[r]()
        if not ok:
            return f"{r}: {why}"
    return None


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
    if not budgets["precommit"] < budgets["prerelease"]:
        bad.append("budget order violated: precommit must be < prerelease")
    for tier in ("precommit", "prerelease"):
        total = sum(c["expected"] for c in tier_checks(checks, tier)
                    if not requirement_needs_infra(c))
        if total > budgets[tier]:
            bad.append(f"{tier}: declared durations sum to {total}s, over the "
                       f"{budgets[tier]}s budget — the tier stopped being what it claims")

    # No dark areas in full.
    covered = {c["area"] for c in checks}
    for area in sorted(AREAS - covered):
        bad.append(f"area {area!r} has no check at all — a dark corner")

    if bad:
        print(f"suite audit: FAIL — {len(bad)} problem(s)")
        for b in bad:
            print(f"  ✗ {b}")
        return 1
    n = {t: len(tier_checks(checks, t)) for t in TIERS}
    print(f"suite audit: ok — {len(checks)} checks "
          f"(precommit {n['precommit']} ⊆ prerelease {n['prerelease']} ⊆ full {n['full']}), "
          f"{len(covered)} areas covered, budgets hold")
    return 0


def requirement_needs_infra(check):
    """Checks whose requirements are inherently absent on some hosts do
    not count against the local budget arithmetic (box/device/ci)."""
    return bool({"box", "device", "ci"} & set(check.get("requires", [])))


# ── run ──────────────────────────────────────────────────────────────

def run_tier(suite, checks, tier, only=None, area=None):
    selected = tier_checks(checks, tier)
    if only:
        selected = [c for c in selected if c["id"] == only]
        if not selected:
            sys.exit(f"suite: no check named {only!r} in tier {tier}")
    if area:
        selected = [c for c in selected if c["area"] == area]

    results = []
    t_start = time.monotonic()
    for c in selected:
        gap = requirement_gap(c)
        if gap:
            results.append((c, "NOT-RUN", 0.0, gap))
            print(f"  ⊘ {c['id']:<22} NOT-RUN  ({gap})")
            continue
        t0 = time.monotonic()
        try:
            # Its own process group, so a timeout kills the whole tree.
            # The first timeout this runner ever fired killed the check's
            # shell and orphaned the servers the check had started — which
            # went on writing runtime files into the repo root, and the
            # NEXT check (rootgate) failed for it. A kill that leaves the
            # children alive converts one red into two, a run apart.
            import os, signal
            proc = subprocess.Popen(
                c["cmd"], shell=True, cwd=ROOT,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True,
            )
            try:
                out, err = proc.communicate(timeout=c["timeout"])
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
                raise
            r = subprocess.CompletedProcess(c["cmd"], proc.returncode, out, err)
            took = time.monotonic() - t0
            if r.returncode == 0:
                results.append((c, "PASS", took, ""))
                print(f"  ✓ {c['id']:<22} {took:6.1f}s")
            else:
                tail = (r.stdout + r.stderr).strip().splitlines()[-6:]
                status = "ADVISORY" if c.get("advisory") else "FAIL"
                results.append((c, status, took, "\n".join(tail)))
                mark = "△" if status == "ADVISORY" else "✗"
                print(f"  {mark} {c['id']:<22} {took:6.1f}s  {status}")
                for line in tail:
                    print(f"      {line[:140]}")
        except subprocess.TimeoutExpired:
            took = time.monotonic() - t0
            results.append((c, "FAIL", took, f"timed out after {c['timeout']}s"))
            print(f"  ✗ {c['id']:<22} {took:6.1f}s  TIMEOUT ({c['timeout']}s)")

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
        import os, signal as sig
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
                            "FAIL", 0.0, "\n".join(leaked_rows[:4])))
        sweep = subprocess.run(["bash", "bench/rootgate.sh"], cwd=ROOT,
                               capture_output=True, text=True)
        if sweep.returncode != 0:
            tail = sweep.stdout.strip().splitlines()[:4]
            results.append(({"id": "exit-hygiene", "area": "hygiene"},
                            "FAIL", 0.0, "\n".join(tail)))
            print(f"  ✗ exit-hygiene: the tier itself left residue behind")
            for line in tail:
                print(f"      {line[:140]}")

    wall = time.monotonic() - t_start
    fails = [r for r in results if r[1] == "FAIL"]
    notrun = [r for r in results if r[1] == "NOT-RUN"]
    advis = [r for r in results if r[1] == "ADVISORY"]
    passed = [r for r in results if r[1] == "PASS"]

    # Real durations land beside the build products so the declared
    # expectations can be corrected from measurement, and cleaning the
    # build cleans this too.
    out = ROOT / f"target/suite-{tier}.json"
    out.parent.mkdir(exist_ok=True)
    out.write_text(json.dumps(
        [{"id": c["id"], "status": s, "seconds": round(t, 1)} for c, s, t, _ in results],
        indent=1))

    budget = suite["budgets"].get(tier)
    print(f"\nsuite {tier}: {len(passed)} passed, {len(fails)} failed, "
          f"{len(advis)} advisory, {len(notrun)} not-run — "
          f"{wall:.0f}s" + (f" (budget {budget}s)" if budget else ""))
    if notrun:
        print("  not run here (loudly, not silently):")
        for c, _, _, why in notrun:
            print(f"    ⊘ {c['id']}: {why}")
    if advis:
        for c, _, _, why in advis:
            print(f"  △ advisory {c['id']}: {why.splitlines()[-1][:120] if why else ''}")
    if fails:
        print("  failed:")
        for c, _, _, _ in fails:
            print(f"    ✗ {c['id']}")
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
    return run_tier(suite, checks, tier, only=only, area=area)


if __name__ == "__main__":
    sys.exit(main())
