#!/usr/bin/env python3
"""Reclaim build and test products, with a report first and rules always.

    python3 tools/clean.py              report what is reclaimable, by class
    python3 tools/clean.py --runtime    runtime residue in the worktree
    python3 tools/clean.py --tmp        kevy scratch stores under $TMPDIR
    python3 tools/clean.py --site       web/dist and web/.ssr (rebuilt by npm run build)
    python3 tools/clean.py --build      cargo dev profile (cargo clean --profile dev)
    python3 tools/clean.py --all        all of the above

The classes exist because each has burned a session:

- **runtime residue** — a test that spawned a server without --dir wrote
  aof-*/dump-* into the repo root; rootgate detects it, this reclaims
  it. Guarded twice: only known runtime patterns, and never anything
  git tracks.
- **tmp scratch** — gates mktemp under $TMPDIR and clean up on exit,
  except when they are killed mid-flight (a suite timeout did exactly
  that); the orphans accumulate silently.
- **site products** — web/dist and web/.ssr are full rebuilds away and
  go stale the moment docs change; a stale dist has fooled a content
  gate before.
- **build** — the dev profile grows without bound (19 GB when this tool
  was written; 4.6 GB reclaimed the first time it was cleaned by hand).
  Release artifacts are kept: they are what the gates run against.

Never touches tracked files. Never guesses: unknown files are reported,
not deleted.
"""

import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent

RUNTIME_GLOBS = [
    "aof-*.aof*", "dump-*.rdb*", "feed-*", "shards.meta",
]
RUNTIME_DIRS = ["tier"]
TMP_GLOBS = ["kevy-*", "kevy_*"]


def du(path: pathlib.Path) -> int:
    if path.is_file():
        return path.stat().st_size
    total = 0
    for p in path.rglob("*"):
        try:
            if p.is_file() and not p.is_symlink():
                total += p.stat().st_size
        except OSError:
            pass
    return total


def human(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024:
            return f"{n:.0f} {unit}"
        n /= 1024
    return f"{n:.1f} TB"


def tracked() -> set:
    out = subprocess.run(["git", "ls-files"], capture_output=True, text=True, cwd=ROOT)
    return set(out.stdout.split())


def runtime_targets():
    tr = tracked()
    found = []
    for g in RUNTIME_GLOBS:
        for p in ROOT.glob(g):
            if str(p.relative_to(ROOT)) not in tr:
                found.append(p)
    for d in RUNTIME_DIRS:
        p = ROOT / d
        if p.is_dir() and not any(str(f.relative_to(ROOT)) in tr for f in p.rglob("*")):
            found.append(p)
    return found


def tmp_targets():
    tmp = pathlib.Path(tempfile.gettempdir())
    found = []
    for g in TMP_GLOBS:
        for p in tmp.glob(g):
            # Only directories that look like our scratch stores — a
            # config file or socket someone else named kevy-* is not ours
            # to delete.
            if p.is_dir():
                found.append(p)
    return found


def site_targets():
    return [p for p in (ROOT / "web/dist", ROOT / "web/.ssr") if p.exists()]


def remove(paths):
    n = 0
    for p in paths:
        if p.is_dir():
            shutil.rmtree(p, ignore_errors=True)
        else:
            p.unlink(missing_ok=True)
        n += 1
    return n


def main():
    args = set(sys.argv[1:])
    do_all = "--all" in args
    classes = {
        "runtime": ("--runtime" in args or do_all, runtime_targets),
        "tmp": ("--tmp" in args or do_all, tmp_targets),
        "site": ("--site" in args or do_all, site_targets),
    }
    report_only = not (do_all or {"--runtime", "--tmp", "--site", "--build"} & args)

    total_reclaimed = 0
    for name, (act, finder) in classes.items():
        targets = finder()
        size = sum(du(p) for p in targets)
        if report_only or not act:
            print(f"  {name:<8} {len(targets):>4} item(s)  {human(size):>9}"
                  + ("" if targets else "  (clean)"))
            continue
        remove(targets)
        total_reclaimed += size
        print(f"  {name:<8} reclaimed {human(size)} ({len(targets)} item(s))")

    tgt = ROOT / "target"
    if tgt.exists():
        size = du(tgt)
        if "--build" in args or do_all:
            r = subprocess.run(["cargo", "clean", "--profile", "dev"],
                               capture_output=True, text=True, cwd=ROOT)
            after = du(tgt)
            print(f"  build    reclaimed {human(size - after)} "
                  f"(dev profile; release kept — the gates run against it)")
            total_reclaimed += size - after
        else:
            print(f"  build    target/ holds {human(size)} "
                  f"(--build cleans the dev profile, keeps release)")

    if report_only:
        print("\n  report only — pass --runtime/--tmp/--site/--build or --all to reclaim")
    elif total_reclaimed:
        print(f"\n  total reclaimed: {human(total_reclaimed)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
