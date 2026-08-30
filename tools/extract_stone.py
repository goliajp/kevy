#!/usr/bin/env python3
"""Can this stone be lifted out and used somewhere else?

`suite/architecture.toml` says a stone is "business-free — any project could
take these". `check_architecture.py` checks that its dependencies point
down the layers, which is necessary and not sufficient: a crate can point
the right way and still be unliftable, because it leans on the workspace
that surrounds it. Nothing has ever tested the claim itself.

This is the operational form of it. Per stone:

1. `cargo package` — the published form, with workspace inheritance already
   resolved, exactly as it reaches crates.io.
2. Unpack it **outside the repository tree**. This is the part that makes
   the test real: cargo walks upward looking for a workspace root, so a
   sandbox anywhere under the repo silently inherits the very thing the
   test is trying to do without. A sandbox in the wrong directory passes
   for the wrong reason.
3. Point the crate's kevy-* dependencies back at local sources via
   `[patch.crates-io]`. A stone's stone-only closure is allowed to come
   with it — that is what "a stone may depend on stone" means — and the
   patch is also what lets an unpublished version be tested at all.
4. Build it, and run its tests.

The verdict is per stone and reported as data. Thresholds belong to
stonegate (G3), which is set from what this measures rather than from
taste.

Run:
  python3 tools/extract_stone.py <crate>
  python3 tools/extract_stone.py --all [--json <out>]
Exit: 0 all lifted, 1 one or more failed, 2 refused.
"""

import json
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
ARCH = ROOT / "suite/architecture.toml"


def refuse(msg):
    print(f"extract: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def stones():
    if not ARCH.exists():
        refuse(f"no {ARCH.relative_to(ROOT)}")
    layers = tomllib.loads(ARCH.read_text())["layers"]
    out = layers.get("stone", [])
    if not out:
        refuse("the architecture map lists no stones")
    return out


def run(cmd, cwd, timeout=1800):
    try:
        p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)
        return p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired:
        return 124, f"timed out after {timeout}s"
    except OSError as e:
        return 125, str(e)


def package(crate, outdir):
    """The published form. --no-verify because step 4 verifies harder."""
    rc, log = run(["cargo", "package", "-p", crate, "--no-verify",
                   "--allow-dirty", "--target-dir", str(outdir)], ROOT)
    if rc != 0:
        return None, log
    hits = sorted(outdir.glob(f"package/{crate}-*.crate"))
    if not hits:
        return None, "cargo package reported success but produced no .crate — " \
                     "exit code answered a different question"
    return hits[-1], log


# `cargo package` strips path dependencies and resolves what is left against
# crates.io. Between a version bump and the publish that follows it, EVERY
# crate with a version-gated sibling is unliftable for that reason alone —
# the version it names does not exist yet, because this is the release that
# creates it. That is a fact about when the measurement was taken, not about
# the crate, and it reads identically to a real failure unless it is named.
# Cargo says it two ways, and only one of them names a version. A crate that
# EXISTS but has no matching version gets "failed to select a version for the
# requirement"; a crate crates.io has never heard of gets "no matching package
# named". kevy-bench is the second kind — it has never been published — and
# the first version of this pattern silently missed both crates that depend
# on it, leaving them looking like ordinary lift failures.
UNRESOLVED = re.compile(
    r'failed to select a version for the requirement `([\w-]+) = "\^?([\d.]+)"`')
UNKNOWN_PKG = re.compile(r'no matching package named `([\w-]+)` found')


def workspace_version():
    """The version this tree carries — what a sibling pin resolves to."""
    doc = tomllib.loads((ROOT / "Cargo.toml").read_text())
    return doc["workspace"]["package"]["version"]


def package_note(log):
    """The line that says what could not be resolved, not just the last line."""
    for line in log.splitlines():
        if ("failed to select a version for the requirement" in line
                or "no matching package named" in line):
            return line.strip()[:200]
    for line in log.splitlines():
        if line.startswith("error"):
            return line.strip()[:200]
    return log.strip().splitlines()[-1][:200] if log.strip() else "package failed"


def unresolved_dep(log, workspace_version):
    """-> {"dep": name, "req": version} when a dependency could not be selected.

    The unknown-package form carries no version, so the requirement is read
    from the manifest's own version — which is exactly the case that matters:
    a sibling this release is about to publish for the first time.
    """
    m = UNRESOLVED.search(log)
    if m:
        return {"dep": m.group(1), "req": m.group(2)}
    m = UNKNOWN_PKG.search(log)
    if m:
        return {"dep": m.group(1), "req": workspace_version, "never_published": True}
    return None


def patch_manifest(cratedir, deps):
    """Point kevy-* dependencies at local sources; keep everything else honest."""
    man = cratedir / "Cargo.toml"
    text = man.read_text()
    lines = ["", "[patch.crates-io]"]
    for d in sorted(deps):
        lines.append(f'{d} = {{ path = "{ROOT / "crates" / d}" }}')
    man.write_text(text + "\n".join(lines) + "\n")


def kevy_deps(cratedir):
    doc = tomllib.loads((cratedir / "Cargo.toml").read_text())
    out = set()
    for sect in ("dependencies", "dev-dependencies", "build-dependencies"):
        out |= {d for d in doc.get(sect, {}) if d.startswith("kevy")}
    return out


def lift(crate, workdir):
    """-> dict describing what happened, without deciding whether it is good."""
    res = {"crate": crate, "packaged": False, "built": False,
           "tested": False, "tests": 0, "note": ""}
    tgt = workdir / f"{crate}-pkg"
    archive, log = package(crate, tgt)
    if archive is None:
        res["note"] = package_note(log)
        res["unresolved"] = unresolved_dep(log, workspace_version())
        return res
    res["packaged"] = True

    # Outside the repo tree: inside it, cargo finds the workspace root and
    # the sandbox tests nothing.
    sandbox = pathlib.Path(tempfile.mkdtemp(prefix=f"stone-{crate}-"))
    assert ROOT not in sandbox.parents and sandbox != ROOT
    try:
        rc, log = run(["tar", "xzf", str(archive)], sandbox)
        if rc != 0:
            res["note"] = "unpack failed"
            return res
        inner = next(iter(sandbox.glob(f"{crate}-*")), None)
        if inner is None:
            res["note"] = "unpacked to nothing"
            return res
        patch_manifest(inner, kevy_deps(inner))
        rc, log = run(["cargo", "build"], inner)
        if rc != 0:
            res["note"] = last_error(log)
            return res
        res["built"] = True
        rc, log = run(["cargo", "test"], inner)
        res["tests"] = sum(int(l.split()[3]) for l in log.splitlines()
                           if l.startswith("test result: ok."))
        if rc != 0:
            res["note"] = last_error(log)
            return res
        res["tested"] = True
        return res
    finally:
        shutil.rmtree(sandbox, ignore_errors=True)


FAILED_TEST = re.compile(r"^test (\S+) \.\.\. FAILED")


def last_error(log):
    """One line saying what went wrong, and for tests, WHICH.

    cargo's own summary for a failing test run is `error: test failed, to
    rerun pass --lib`, and taking the first `error` line gives exactly that
    — a report that a stone did not lift, with the reason left in a log
    nobody kept. Diagnosing one cost a full CI round: the stone report said
    `tested: false, tests: 0` and the note said nothing more.

    So a test failure names its tests and carries the first panic line.
    A build error still reports the first `error`, which for a compile
    failure is the thing itself.
    """
    failed = [m.group(1) for line in log.splitlines()
              if (m := FAILED_TEST.match(line))]
    if failed:
        shown = ", ".join(failed[:4])
        more = f" (+{len(failed) - 4} more)" if len(failed) > 4 else ""
        # cargo prints `panicked at <file>:<line>:` and puts the message on
        # the NEXT line, so the location alone is half a sentence.
        lines = log.splitlines()
        why = ""
        for i, l in enumerate(lines):
            if "panicked at" in l:
                where = l.split("panicked at", 1)[1].strip().rstrip(":")
                msg = lines[i + 1].strip() if i + 1 < len(lines) else ""
                why = f"{where} {msg}".strip()
                break
        tail = f" — {why}" if why else ""
        return f"{len(failed)} test(s) failed: {shown}{more}{tail}"[:200]
    errs = [l for l in log.splitlines() if l.startswith("error")]
    return (errs[0] if errs else log.strip().splitlines()[-1] if log.strip() else "?")[:200]


def main():
    args = sys.argv[1:]
    if not args:
        refuse("usage: extract_stone.py <crate> | --all [--json <out>]")
    targets = stones() if args[0] == "--all" else [args[0]]
    if args[0] != "--all" and args[0] not in stones():
        refuse(f"{args[0]} is not classified as a stone in suite/architecture.toml")

    results = []
    with tempfile.TemporaryDirectory(prefix="stone-pkg-") as work:
        for c in targets:
            r = lift(c, pathlib.Path(work))
            results.append(r)
            mark = "✓" if r["tested"] else ("~" if r["built"] else "✗")
            print(f"  {mark} {c:<16} packaged={r['packaged']} built={r['built']} "
                  f"tested={r['tested']} tests={r['tests']}"
                  + (f"  {r['note']}" if r["note"] else ""))

    if "--json" in args:
        out = pathlib.Path(args[args.index("--json") + 1]).resolve()
        out.write_text(json.dumps(results, indent=2) + "\n")
        print(f"  wrote {out}")

    ok = sum(1 for r in results if r["tested"])
    print(f"extract: {ok}/{len(results)} stones lift and pass their tests outside the workspace")
    return 0 if ok == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
