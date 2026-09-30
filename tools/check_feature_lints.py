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

**Features taken away.** A crate built with fewer than its defaults — the
minimal cut an embedded user asks for, or one default feature on top of
it — compiles code whose imports and lint expectations were only ever
checked with every default on. `kevy-embedded --no-default-features
--features core` stopped building on an unused import, and nothing said
so. These run on the library and binaries only: `--all-targets` pulls
in dev-dependencies, and a dev-dependency that asks for the defaults
puts them straight back.

The lists come from `cargo metadata`, so a crate added tomorrow, or a
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


# Crates whose empty feature set is not a configuration, with the smallest
# one that is. Same staleness rule as EXCLUSIVE.
MINIMAL = {
    "kevy-store": {
        "reason": "without `std` the TTL clock must be host-fed; lib.rs refuses "
        "to build without `external-clock`",
        "set": ["alloc", "external-clock"],
    },
    "kevy-embedded": {
        "reason": "`core` is the empty marker its Cargo.toml names the minimal "
        "archetype with; spelled the way its users spell it",
        "set": ["core"],
    },
}


def closure(features: dict, start) -> set:
    seen = set(start)
    stack = list(seen)
    while stack:
        for dep in features.get(stack.pop(), []):
            if not dep.startswith("dep:") and dep in features and dep not in seen:
                seen.add(dep)
                stack.append(dep)
    return seen


def default_closure(features: dict) -> set:
    return closure(features, features.get("default", []))


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

    # Axis 3 — features taken away: the minimal set, then each default
    # feature alone on top of it. Library and binaries only (see above).
    for pkg in sorted(meta["packages"], key=lambda p: p["name"]):
        name, features = pkg["name"], pkg["features"]
        defaults = features.get("default", [])
        if not defaults or name in EXCLUSIVE:
            continue
        base = MINIMAL.get(name, {}).get("set", [])
        whole = default_closure(features)
        # a default feature that already implies every other is the
        # default build, which axis 1 lints
        sets = [base] + [
            sorted({*base, f}) for f in defaults
            if f not in base and closure(features, [*base, f]) != whole
        ]
        for s in sets:
            args = ["--no-default-features", *(["--features", ",".join(s)] if s else [])]
            label = "minimal" if s == base else "minimal + " + ",".join(x for x in s if x not in base)
            jobs.append((name, f"{label} [{','.join(s) or 'none'}], lib", args))

    # The floor. Finding little to lint is what this script looks like
    # when `cargo metadata` changes shape under it, and that reads exactly
    # like a clean tree. The workspace has 47 members and 5 crates with
    # off-by-default features; anything near those numbers is the tree,
    # anything far below them is the device.
    if len(jobs) < 45:
        print(f"FAIL: only {len(jobs)} configurations found; the tree has far more")
        return 1

    names = {p["name"] for p in meta["packages"]}
    stale = [c for c in (*EXCLUSIVE, *MINIMAL) if c not in names]
    if stale:
        print(f"FAIL: EXCLUSIVE / MINIMAL name crates that are gone: {stale}")
        return 1
    feats = {p["name"]: p["features"] for p in meta["packages"]}
    gone = [(c, f) for c, m in MINIMAL.items() if c in feats for f in m["set"] if f not in feats[c]]
    if gone:
        print(f"FAIL: MINIMAL names features that are gone: {gone}")
        return 1

    bad = []
    for crate, label, args in jobs:
        scope = ["--lib", "--bins"] if label.endswith(", lib") else ["--all-targets"]
        cmd = ["cargo", "clippy", "-p", crate, *scope, *target, *args,
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
