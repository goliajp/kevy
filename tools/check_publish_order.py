#!/usr/bin/env python3
"""Verify the release workflow's publish chain is a real topological order.

`cargo publish -p C` resolves every version-gated dependency of C against
crates.io, so each such dependency must already be published. That
includes **dev- and build-dependencies**, not only normal ones: a
dev-dependency written as `{ path = "...", version = "5.0.0" }` is
verified exactly like a normal one.

The workflow already asserts the chain COVERS every publishable crate.
Set coverage is not order: the v5.0.0 release broke twice on ordering
alone — once because a new crate sat before the crate it depends on, and
once because `kevy-cluster-rw`'s dev-dependency on `kevy-rt` put it
ahead of `kevy-rt`. Both cost a mid-release retag, which is expensive
because crates.io publishes cannot be undone.

Run: python3 tools/check_publish_order.py
"""

import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/release.yml"


def chains_from_workflow(text):
    """Every crate list in the workflow, as (label, [crate, ...]).

    Two lists exist — the self-check's `chain=` and the publish loop's
    `for c in` — and they must agree, so both are returned and compared.
    """
    out = []
    for label, pattern in (
        ("self-check", r"chain=\$\(printf '%s\\n' \\\n(.*?)\| sort -u\)"),
        ("publish loop", r"for c in \\\n(.*?); do"),
    ):
        m = re.search(pattern, text, re.S)
        if not m:
            sys.exit(f"check_publish_order: could not find the {label} crate list")
        crates = m.group(1).replace("\\", " ").split()
        out.append((label, crates))
    return out


def workspace_deps():
    """{crate: {dep, ...}} over workspace members, version-gated deps only."""
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT
        )
    )
    members = {p["name"] for p in meta["packages"]}
    publishable = {p["name"] for p in meta["packages"] if p.get("publish") is None}
    deps = {}
    for pkg in meta["packages"]:
        wanted = set()
        for d in pkg["dependencies"]:
            # A path-only dependency (req "*") is stripped on publish and
            # imposes no ordering; anything carrying a version is resolved
            # against crates.io, whatever its kind (normal/dev/build).
            if d["name"] in members and d["req"] != "*":
                wanted.add(d["name"])
        deps[pkg["name"]] = wanted & members
    return deps, publishable



def unversioned_path_deps():
    """Path dependencies with no version, in a crate that gets published.

    `cargo publish` refuses them: the path is stripped when the crate is
    uploaded, so a dependency with no version requirement has nothing left
    to resolve. It refuses dev-dependencies too — and a test-only crate is
    exactly where the omission is easy to make, because everything builds
    and tests fine locally.

    This gate verified the *order* of the chain and said nothing about
    whether each crate could be published at all, so a `kevy-testnet =
    { path = "../kevy-testnet" }` written without a version passed here
    and failed eleven crates into the real publish — after eleven had
    already gone to crates.io, where nothing can be taken back.
    """
    bad = []
    for manifest in sorted(ROOT.glob("crates/*/Cargo.toml")):
        text = manifest.read_text(encoding="utf-8")
        if "publish = false" in text:
            continue
        section = None
        for line_no, line in enumerate(text.splitlines(), 1):
            st = line.strip()
            if st.startswith("["):
                section = st
                continue
            # Only real and build dependencies. cargo publish strips
            # dev-dependencies entirely, so a version-less one is fine
            # there — kevy-store and kevy-vlog have carried exactly that
            # for many releases and published every time. Flagging them
            # would be this gate inventing a rule cargo does not have.
            if section not in ("[dependencies]", "[build-dependencies]"):
                continue
            if "path = " in st and "version" not in st and not st.startswith("#"):
                name = st.split("=")[0].strip()
                bad.append((manifest.relative_to(ROOT), line_no, name, section))
    return bad


def main():
    text = WORKFLOW.read_text()
    lists = chains_from_workflow(text)
    (label_a, chain_a), (label_b, chain_b) = lists
    if chain_a != chain_b:
        only_a = [c for c in chain_a if c not in chain_b]
        only_b = [c for c in chain_b if c not in chain_a]
        print(f"FAIL: the {label_a} and {label_b} lists disagree")
        if only_a or only_b:
            print(f"  only in {label_a}: {only_a}")
            print(f"  only in {label_b}: {only_b}")
        else:
            print("  same crates, different order")
        return 1
    chain = chain_a

    deps, publishable = workspace_deps()
    problems = []

    missing = publishable - set(chain)
    extra = set(chain) - publishable
    for c in sorted(missing):
        problems.append(f"publishable crate never published: {c}")
    for c in sorted(extra):
        problems.append(f"chain lists a crate that is not publishable: {c}")

    position = {c: i for i, c in enumerate(chain)}
    for crate in chain:
        for dep in sorted(deps.get(crate, ())):
            if dep not in position:
                problems.append(
                    f"{crate} depends on {dep}, which the chain never publishes"
                )
            elif position[dep] > position[crate]:
                problems.append(
                    f"{crate} (#{position[crate]}) is published before its "
                    f"dependency {dep} (#{position[dep]})"
                )

    if problems:
        print("FAIL: publish chain is not a valid topological order")
        for p in problems:
            print(f"  {p}")
        print("\nfix: move each crate after every dependency listed above")
        return 1

    unversioned = unversioned_path_deps()
    if unversioned:
        print(f"check_publish_order: FAIL — {len(unversioned)} path dependency(ies) with no version")
        for rel, line_no, name, section in unversioned:
            print(f"  \u2717 {rel}:{line_no}  {name} in {section}")
        print("\ncargo publish strips the path and has nothing left to resolve.")
        print("An unpublished helper still needs a version beside its path, or the")
        print("crate depending on it cannot be published at all.")
        return 1

    edges = sum(len(deps.get(c, ())) for c in chain)
    print(
        f"ok: publish chain is a topological order — {len(chain)} crates, "
        f"{edges} version-gated workspace edges (dev and build included)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
