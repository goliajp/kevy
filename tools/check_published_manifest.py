#!/usr/bin/env python3
"""Is the crate crates.io serves at this version the one the tree would publish?

`cargo publish` answers "already exists" by version number alone. That is
the right answer when a release is being retried and the wrong one when
the manifest changed under a version that did not move — which is how
kevy-client 2.2.0 pinned its siblings at 6.x in the tree for three whole
releases while crates.io kept serving a 2.2.0 that pinned them at ^5.0.
The publish loop read "already exists" as "already done" three times.

This asks the registry what it holds and compares it with what `cargo
metadata` says the package carries: every dependency's name, requirement,
kind and optionality. Path-only dependencies are left out on both sides,
because `cargo package` drops them from the manifest it uploads.

    python3 tools/check_published_manifest.py <crate> <version>

exit 0  the registry's manifest is the tree's; skipping the publish is honest
exit 1  they differ (printed); the crate must ship under a new version
exit 2  the registry gave no answer, which is not a pass
"""
import json
import pathlib
import subprocess
import sys
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
UA = "kevy-release-check (https://github.com/goliajp/kevy)"


def registry_deps(crate: str, version: str):
    """(name, req, kind, optional) for every dependency crates.io records."""
    url = f"https://crates.io/api/v1/crates/{crate}/{version}/dependencies"
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            data = json.load(r)
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise
    return {
        (d["crate_id"], d["req"], d["kind"], bool(d.get("optional")))
        for d in data["dependencies"]
    }


def tree_deps(crate: str, meta=None):
    """The same tuples for the workspace member, as `cargo package` would ship them."""
    if meta is None:
        meta = json.loads(subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=ROOT, capture_output=True, text=True, check=True).stdout)
    pk = next(p for p in meta["packages"] if p["name"] == crate)
    out = set()
    for d in pk["dependencies"]:
        if d.get("path") and d["req"] == "*":
            continue  # path-only: dropped by `cargo package`
        out.add((d["name"], d["req"], d.get("kind") or "normal", bool(d.get("optional"))))
    return out, pk["version"]


def compare(crate: str, version: str, meta=None):
    """(verdict, lines). verdict: True same, False differs, None no answer."""
    have = registry_deps(crate, version)
    if have is None:
        return None, [f"{crate} {version}: crates.io has no such version"]
    want, tree_version = tree_deps(crate, meta)
    lines = []
    if tree_version != version:
        lines.append(f"{crate}: tree declares {tree_version}, asked about {version}")
    for name, req, kind, opt in sorted(want - have):
        lines.append(f"  tree has      {name} {req} ({kind}{', optional' if opt else ''}) — registry does not")
    for name, req, kind, opt in sorted(have - want):
        lines.append(f"  registry has  {name} {req} ({kind}{', optional' if opt else ''}) — tree does not")
    return (not lines), lines


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    crate, version = sys.argv[1], sys.argv[2]
    ok, lines = compare(crate, version)
    if ok is None:
        print("\n".join(lines))
        print("The registry did not answer. That is not a pass.")
        return 2
    if ok:
        print(f"ok: crates.io {crate} {version} carries the tree's manifest")
        return 0
    print(f"{crate} {version} on crates.io is not what the tree would publish:")
    print("\n".join(lines))
    print("A version that did not move under a manifest that did is a crate "
          "the world cannot get. Bump and republish.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
