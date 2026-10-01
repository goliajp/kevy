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

The set is the one git tracks, because "committed" is what the question is
about and git is what defines it. That is not the set on disk: the Tauri
plugin and the wasm example keep their locks out of every checkout with
their own .gitignore, the way a library does, so a filesystem walk found 23
and spent two of them on locks no checkout has. Asking git also costs
nothing — an rglob from the root took 10s to walk `target/` and
`node_modules` and throw away 199 of the 227 hits it brought back.

The queries run in a pool because each one takes cargo's package-cache lock
and any other cargo on the machine — an editor's rust-analyzer, most often —
holds it in bursts. Sequentially each query waits out its own burst: against
a holder cycling once a second, 21 queries took 13.6s, where the same 21 in
a pool waited once, together, for 4.0s. Uncontended it is 0.82s against
0.23s.

A slow run explains itself, because the first one was read as a regression:
147s of wall clock over 5s of CPU was an editor holding the package cache,
and nothing about a lock. The witness is that ratio and not cargo's
"Blocking waiting for file lock" line, which a pool provokes against itself
— at 8 workers all 21 queries print it while the whole run takes 0.2s.

Run: python3 tools/check_lockfiles.py
Exit: 0 pass, 1 a lock is stale or uncommitted, 2 refused.
"""

import os
import pathlib
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

ROOT = pathlib.Path(__file__).resolve().parent.parent
# 21 today. A floor, so a selector that stops finding them fails here instead
# of passing over an empty set. Raise it when a crate brings its own lock.
MIN_LOCKS = 20
# Bounded by the package-cache lock, not by CPU: 8 was enough to collapse the
# contended case to a single wait, and keeps the process count predictable.
WORKERS = 8
# Past this the run is worth explaining. The work itself is 0.2s of wall and
# under 1s of CPU, so ten seconds is an order of magnitude above any honest
# run and an order below the tier's timeout.
SLOW_SECONDS = 10


def children_cpu():
    """CPU the cargo subprocesses have used, user plus system."""
    t = os.times()
    return t.children_user + t.children_system


def refuse(msg):
    print(f"lockgate: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def git(*args):
    try:
        p = subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True)
    except (OSError, subprocess.CalledProcessError) as e:
        refuse(f"git {args[0]} failed: {e}")
    return p.stdout


def unclean_locks():
    """Lock files git has something to say about: changed, or never added.

    This is the failure mode that actually happened: the working tree's lock
    was correct and the COMMITTED one was stale, so every local command
    passed — including a --locked check, which reads the working tree — and
    the release image, which reads the checkout, did not. A modified lock is
    a committed lock that no longer matches; an untracked one is a lock the
    checkout will not have at all.
    """
    changed, untracked = [], []
    for line in git("status", "--porcelain", "--", "*Cargo.lock").splitlines():
        if line.strip():
            (untracked if line.startswith("??") else changed).append(line[3:].strip())
    return changed, untracked


def manifests():
    """The manifest beside every lock file git tracks."""
    locks = [p for p in git("ls-files", "-z", "--", "*Cargo.lock").split("\0") if p]
    if len(locks) < MIN_LOCKS:
        refuse(f"git tracks {len(locks)} lock files, fewer than {MIN_LOCKS}; the selector is broken")
    return [(ROOT / lock).parent / "Cargo.toml" for lock in locks]


def check(man):
    """The release image's question, for one manifest; why it failed, or None."""
    if not man.exists():
        return f"{man.name} is not here, but its lock is committed"
    # No `--no-deps`. With it, cargo answers from the manifest alone and
    # never has to consult the lock, so every stale one passed: 21 of the
    # 22 locks here failed `--locked` the moment the flag came off, some
    # pinning kevy crates four majors back. The flag was asking the
    # release image's question with the part that reads the lock removed.
    p = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--manifest-path", str(man)],
        cwd=ROOT, capture_output=True, text=True,
    )
    if p.returncode == 0:
        return None
    return next((l.strip() for l in p.stderr.splitlines() if "error" in l.lower()),
                p.stderr.strip()[:120])


def main():
    changed, untracked = unclean_locks()
    mans = manifests()
    started, cpu_before = time.monotonic(), children_cpu()
    with ThreadPoolExecutor(min(WORKERS, len(mans))) as pool:
        answers = list(pool.map(check, mans))
    elapsed, cpu = time.monotonic() - started, children_cpu() - cpu_before
    stale = [(m.relative_to(ROOT), why) for m, why in zip(mans, answers) if why]

    if elapsed > SLOW_SECONDS:
        print(f"lockgate: this run took {elapsed:.0f}s over {cpu:.1f}s of CPU — the difference "
              f"is cargo's package cache,")
        print("  held by something else on this machine. It says nothing about a lock.")
    if changed or untracked:
        print(f"lockgate: FAIL — {len(changed) + len(untracked)} lock file(s) are not what a "
              f"checkout would hold")
        for d in changed:
            print(f"  {d}: changed in the working tree")
        for d in untracked:
            print(f"  {d}: never added")
        print("  The committed lock is what `--locked` reads. A correct one in the")
        print("  working tree does not help the release image.")
        return 1
    if stale:
        print(f"lockgate: FAIL — {len(stale)} lock file(s) do not match their manifest")
        for m, why in stale:
            print(f"  {m}: {why}")
        print("  fix: run a cargo command in that directory and COMMIT the lock")
        return 1
    print(f"lockgate: PASS — {len(mans)} committed lock file(s) match their manifests, "
          f"none uncommitted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
