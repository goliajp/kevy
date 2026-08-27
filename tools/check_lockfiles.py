#!/usr/bin/env python3
"""Every committed lock file matches its manifest.

`cargo build --locked` refuses to update a lock file, and the release image
builds that way — so a lock that has drifted from its manifest fails at the
Docker step and nowhere earlier. That is a `full`-tier CI job, which is a
long way to travel for a one-line diff.

It drifted for a real reason, not carelessness: every ordinary cargo command
regenerates the lock silently, so `cargo metadata`, `cargo test`,
`check_publish_order.py` and `check_package.py` all pass against a stale
committed lock while quietly fixing it in the working tree. The only tool
that objects is one that is forbidden to fix it.

So this asks the question the release image asks, everywhere a lock file
lives, and it is cheap enough for precommit.

Run: python3 tools/check_lockfiles.py
Exit: 0 pass, 1 a lock is stale, 2 refused.
"""

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
# Below this, the search is broken rather than the tree bare.
MIN_LOCKS = 1


def refuse(msg):
    print(f"lockgate: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def uncommitted_locks():
    """Lock files with working-tree changes.

    This is the failure mode that actually happened: the working tree's lock
    was correct and the COMMITTED one was stale, so every local command
    passed — including a --locked check, which reads the working tree — and
    the release image, which reads the checkout, did not. A modified lock is
    a committed lock that no longer matches.
    """
    try:
        out = subprocess.run(["git", "status", "--porcelain", "--", "*Cargo.lock"],
                             cwd=ROOT, capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        refuse(f"git status failed: {e}")
    return [l[3:].strip() for l in out.splitlines() if l.strip()]


def manifests():
    """Every directory holding a Cargo.lock, as a manifest path."""
    out = []
    for lock in sorted(ROOT.rglob("Cargo.lock")):
        if "target" in lock.parts or "node_modules" in lock.parts:
            continue
        # Comparative-benchmark scaffolding: these manifests point at the
        # crates through paths that only resolve inside their own probe
        # layout, so `cargo metadata` cannot load them from here. Not stale,
        # not checkable, and skipped by name rather than by a wildcard that
        # would quietly grow.
        if ".claude/perfs/comparative" in str(lock):
            continue
        man = lock.parent / "Cargo.toml"
        if man.exists():
            out.append(man)
    if len(out) < MIN_LOCKS:
        refuse(f"found {len(out)} lock files; the selector is broken")
    return out


def main():
    dirty = uncommitted_locks()
    stale = []
    mans = manifests()
    for man in mans:
        p = subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1",
             "--no-deps", "--manifest-path", str(man)],
            cwd=ROOT, capture_output=True, text=True,
        )
        if p.returncode != 0:
            why = next((l.strip() for l in p.stderr.splitlines()
                        if "error" in l.lower()), p.stderr.strip()[:120])
            stale.append((man.relative_to(ROOT), why))

    if dirty:
        print(f"lockgate: FAIL — {len(dirty)} lock file(s) changed but not committed")
        for d in dirty:
            print(f"  {d}")
        print("  The committed lock is what `--locked` reads. A correct one in the")
        print("  working tree does not help the release image.")
        return 1
    if stale:
        print(f"lockgate: FAIL — {len(stale)} lock file(s) do not match their manifest")
        for m, why in stale:
            print(f"  {m}: {why}")
        print("  fix: run a cargo command in that directory and COMMIT the lock")
        return 1
    print(f"lockgate: PASS — {len(mans)} lock file(s) match their manifests, "
          f"none uncommitted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
