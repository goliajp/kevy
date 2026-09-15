#!/usr/bin/env python3
"""Lint the configurations `cargo clippy --workspace` never compiles.

`--workspace --all-targets` lints one configuration: every crate carrying
the UNION of the features anything in the tree asks of it. Two whole
classes of real configuration are invisible to that, and this script is
both of them.

**Features the union does not reach.** A feature that is off by default
is compiled by nobody, and `--all-features` across the workspace is not
an available substitute: kevy-client-async's three runtime features are
mutually exclusive by design and its lib.rs says so with a
`compile_error!`. What this hid until the script was written: `unsafe
impl GlobalAlloc for KevyAlloc` — the most safety-critical impl in the
tree — carried no SAFETY argument, behind kevy-alloc's off-by-default
`global`. The workspace was clippy-zero on two platforms at the time. It
was a green that covered one feature set, the same shape as the green
that covered one platform.

**Features the union ADDS.** A crate built alone gets only what it asks
for. kevy-wasm takes kevy-embedded without `tier`, and that — the
configuration the wasm package actually ships — had a real clippy error
in it that the workspace step could not see. CI grew a hand-written step
for that one crate. Hand-written is how `global` was missed: the list has
to be derived, so every crate is linted alone here.

Both lists come from `cargo metadata`, so a crate added tomorrow, or a
feature added tomorrow, is covered tomorrow with no edit.

`--target <triple>` passes through, for the same reason the axes exist:
the host is one more thing a green can quietly be about. Run from macOS,
`--target x86_64-unknown-linux-gnu` is what makes the reactor's
Linux-only code participate at all.
"""

import json
import subprocess
import sys

# Crates whose features cannot be enabled together, with the sets to use
# instead. A name here that no longer has those features is an error:
# a table that outlives what it describes silently stops covering it.
EXCLUSIVE = {
    "kevy-client-async": {
        "reason": "the three runtimes are mutually exclusive by design "
        "(lib.rs: 'Pick exactly one of tokio, smol, or async-std')",
        "sets": [["tokio"], ["smol"], ["async-std"]],
    }
}


def default_closure(features: dict) -> set:
    seen = set(features.get("default", []))
    stack = list(seen)
    while stack:
        for dep in features.get(stack.pop(), []):
            if not dep.startswith("dep:") and dep in features and dep not in seen:
                seen.add(dep)
                stack.append(dep)
    return seen


def main() -> int:
    target: list[str] = []
    argv = sys.argv[1:]
    if argv[:1] == ["--target"] and len(argv) == 2:
        target = ["--target", argv[1]]
    elif argv:
        print(f"usage: {sys.argv[0]} [--target <triple>]")
        return 2

    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )

    jobs = []  # (crate, human label, clippy args)

    # Axis 1 — every crate alone, with its own default features. This is
    # what the crate is when someone depends on it, and it is NOT what
    # the workspace build lints.
    for pkg in sorted(meta["packages"], key=lambda p: p["name"]):
        jobs.append((pkg["name"], "alone, default features", []))

    # Axis 2 — the feature sets nothing turns on.
    for pkg in sorted(meta["packages"], key=lambda p: p["name"]):
        name, features = pkg["name"], pkg["features"]
        off = sorted(f for f in features if f != "default" and f not in default_closure(features))
        if not off:
            continue
        if name in EXCLUSIVE:
            for s in EXCLUSIVE[name]["sets"]:
                jobs.append((name, ",".join(s), ["--no-default-features", "--features", ",".join(s)]))
        else:
            jobs.append((name, "all-features (" + ",".join(off) + ")", ["--all-features"]))

    # The floor. Finding little to lint is what this script looks like
    # when `cargo metadata` changes shape under it, and that reads exactly
    # like a clean tree. The workspace has 47 members and 5 crates with
    # off-by-default features; anything near those numbers is the tree,
    # anything far below them is the device.
    if len(jobs) < 45:
        print(f"FAIL: only {len(jobs)} configurations found; the tree has far more")
        return 1

    stale = [c for c in EXCLUSIVE if c not in {p["name"] for p in meta["packages"]}]
    if stale:
        print(f"FAIL: EXCLUSIVE names crates that are gone: {stale}")
        return 1

    bad = []
    for crate, label, args in jobs:
        cmd = ["cargo", "clippy", "-p", crate, "--all-targets", *target, *args,
               "--", "-D", "warnings"]
        r = subprocess.run(cmd, capture_output=True, text=True)
        mark = "ok " if r.returncode == 0 else "FAIL"
        if r.returncode != 0 or not label.startswith("alone"):
            print(f"  {mark} {crate}  [{label}]")
        if r.returncode != 0:
            bad.append((crate, label, r.stderr))

    where = f" for {target[1]}" if target else ""
    print(f"\n{len(jobs)} configurations linted{where}, {len(bad)} failing")
    for crate, label, err in bad:
        print(f"\n─── {crate} [{label}] ───\n{err}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
