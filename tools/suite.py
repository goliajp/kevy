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

import os
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

def _have_binaries(profile):
    """Build the binaries, rather than judge whether those on disk are current.

    Existence alone was not enough: a `target/debug/kevy` from an earlier
    checkout satisfied it while `doc-toml` used that binary to load the
    documentation's config blocks, and reported `packed_rows` as an unknown
    `[server]` key — hours after the source in the same tree had gained it.
    On the box that binary was rebuilt later in the same tier, by
    `workspace-tests`, which is why the same check passed on a re-run.

    The first fix compared the binary's mtime against the sources, and was
    wrong in a way worth recording: cargo decides freshness by hashing
    content, so a `git checkout` or a merge moves mtimes without changing
    anything, and the check cried stale after every branch operation. A gate
    that cries wolf gets worked around.

    So this asks cargo, which is the tool whose job that is. A fresh tree
    costs ~0.1 s; a stale one costs a build, which is the honest price of
    the guarantee.

    Both binaries, because the gates that name this need both and the
    guarantee only ever covered one. A two-day-old `target/release/kevy-cli`
    failed cookbook, crossgate and site-commands in one prerelease run, each
    in a way that reads as a defect in the tree: cookbook reported the CLI
    refusing `-e`, an option the same tree had added. The requirement was
    called `server-*` while ten checks under it drive kevy-cli, which is why
    it is not called that any more.
    """
    flags = ["--release"] if profile == "release" else []
    for pkg, bin_name in (("kevy", "kevy"), ("kevy-cli", "kevy-cli")):
        r = subprocess.run(["cargo", "build", "-p", pkg, "--bin", bin_name, "--quiet", *flags],
                           cwd=ROOT, capture_output=True, text=True)
        if r.returncode != 0:
            tail = (r.stderr or r.stdout).strip().splitlines()
            why = tail[-1] if tail else f"cargo build --bin {bin_name} failed ({r.returncode})"
            return False, f"target/{profile}/{bin_name} does not build: {why[:120]}"
        if not (ROOT / f"target/{profile}/{bin_name}").exists():
            return False, f"cargo build succeeded but target/{profile}/{bin_name} is not there"
    return True, ""


def _have_linux():
    import platform
    return (platform.system() == "Linux"), "not a Linux host"


def _have_box():
    import platform
    if platform.system() == "Linux" and (os_cpus() or 0) >= 16:
        return True, ""
    return False, "needs the 16-core Linux box (quiet, core-pinnable)"


def os_cpus():
    try:
        return len(os.sched_getaffinity(0))  # type: ignore[attr-defined]
    except AttributeError:
        return os.cpu_count()


def _have_node():
    if shutil.which("node"):
        return True, ""
    return False, "node is not on PATH"


def _have_chromium():
    """A browser, not the library that drives one.

    This asked whether `web/node_modules/playwright-core` existed, which
    is a different question with a different answer: Playwright installs
    its browsers separately from itself. On the bench box — library
    present, browser absent — the requirement read as satisfied and
    sitegate then failed inside `chromium.launch`, leaving a stack trace
    that says nothing about the site and everything about the machine.
    A missing requirement is supposed to be a loud NOT-RUN.

    `web/find-browser.mjs` is the one place that knows where a browser
    is; run directly it prints the path or exits 1, which is exactly this
    question. Asking it rather than restating its rules here keeps the
    runner and `verify.mjs` from ever disagreeing about what counts.
    """
    if not shutil.which("node"):
        return False, "node is not on PATH, so no browser can be located"
    probe = ROOT / "web/find-browser.mjs"
    if not probe.exists():
        return False, f"{probe.relative_to(ROOT)} is missing"
    r = subprocess.run(
        ["node", str(probe)], capture_output=True, text=True, cwd=ROOT, timeout=30
    )
    if r.returncode == 0 and r.stdout.strip():
        return True, ""
    return False, "no Chromium: set CHROME_PATH, or `npx playwright install chromium` in web/"


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
    if os.environ.get("KEVY_DEVICE") == "1":
        return True, ""
    return False, "no device session (set KEVY_DEVICE=1 on the machine that has one)"


def children_cpu():
    """CPU the checks' own subprocesses have used, user plus system.

    A duration is only a cost when the machine was the check's. `cargo test
    -p kevy --test differential_server_vs_embedded` took 5.9s in prerelease
    and 578.2s in full, from the same tree minutes apart: run by hand it is
    0.14s of test inside 27s of wall and 1.9s of CPU, because another cargo
    on this machine held the target lock. Recording wall alone makes that row
    read as a check that costs ten minutes.
    """
    t = os.times()
    return t.children_user + t.children_system


def requirement_gap(check):
    """The first unmet requirement, or None."""
    for r in check.get("requires", []):
        ok, why = {
            "binaries-debug": lambda: _have_binaries("debug"),
            "binaries-release": lambda: _have_binaries("release"),
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
    for r in sorted(set(declared) - used):
        bad.append(f"requirement {r!r} is declared but no check asks for it — "
                   f"the list rotted")

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

    results, cpu_of = [], {}
    t_start = time.monotonic()
    for c in selected:
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
                proc.wait()
                raise
            r = subprocess.CompletedProcess(c["cmd"], proc.returncode, out, err)
            took = time.monotonic() - t0
            cpu_of[c["id"]] = children_cpu() - cpu0
            if r.returncode == 0:
                results.append((c, "PASS", took, "", True))
                print(f"  ✓ {c['id']:<22} {took:6.1f}s")
            elif r.returncode == 2 and c.get("skip_is_exit_2"):
                # Exit 2 means "I did not answer the question", not "the answer
                # is no" — a gate that skipped an outward call and says so must
                # not read as a failure, and must not read as a pass either.
                # The row carries the reason, the way a NOT-RUN does.
                why = ((r.stdout + r.stderr).strip().splitlines() or ["exit 2"])[-1]
                results.append((c, "SKIPPED", took, why, False))
                print(f"  ⊘ {c['id']:<22} {took:6.1f}s  SKIPPED — {why[:80]}")
            else:
                tail = (r.stdout + r.stderr).strip().splitlines()[-6:]
                status = "ADVISORY" if c.get("advisory") else "FAIL"
                results.append((c, status, took, "\n".join(tail), True))
                mark = "△" if status == "ADVISORY" else "✗"
                print(f"  {mark} {c['id']:<22} {took:6.1f}s  {status}")
                for line in tail:
                    print(f"      {line[:140]}")
        except subprocess.TimeoutExpired:
            took = time.monotonic() - t0
            cpu_of[c["id"]] = children_cpu() - cpu0
            results.append((c, "TIMEOUT", took, f"timed out after {c['timeout']}s", False))
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
    skipped = [r for r in results if r[1] == "SKIPPED"]

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
    # decomposition. A NOT-RUN row never ran; a SKIPPED one ran only as far
    # as refusing to answer; the two FAIL rows this runner synthesises after
    # the tier carry no duration at all. None of those four is what the
    # check costs, and all four used to be filed as though they were.
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
          f"{len(skipped)} skipped, {len(notrun)} not-run — "
          f"{wall:.0f}s" + (f" (budget {budget}s)" if budget else ""))
    # The tally must account for every check that was selected. It did not:
    # a TIMEOUT and a SKIPPED were in neither the counts nor the failed list,
    # so `workspace-tests` hit its 5400s ceiling and 53 checks were reported
    # as "43 passed, 2 failed, 1 advisory, 5 not-run". Eleven short of the
    # truth, in a line whose whole job is to be the truth.
    counted = len(passed) + len(fails) + len(timeouts) + len(advis) + len(skipped) + len(notrun)
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
    return run_tier(suite, checks, tier, only=only, area=area)


if __name__ == "__main__":
    sys.exit(main())
