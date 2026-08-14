#!/usr/bin/env python3
"""Does the wasm about to be deployed agree with the one already published?

Deploying the site puts a wasm artifact on kevy.golia.jp that announces a
version in the terminal's title bar. npm serves a package announcing the
same version. On 2026-08-14 those were two different builds — 744 KB
without `IDX.CREATE` on npm, 1442 KB with it on the site — both calling
themselves 5.1.0. A reader who tried the terminal and then installed the
package would get less than the page showed, and nothing said so.

Between releases the tree is *supposed* to be ahead of npm; that is what a
tree is. This is not a CI gate for that reason — it would be red from the
first engine change after every release, and a permanently red gate is a
gate nobody reads. It belongs where the divergence becomes public: before
`rsync web/dist/ t01:/apps/kevy/web/`.

What to do when it reports a difference is a release decision, not a
deploy one: publish a new version of @goliapkg/kevy so the two agree
again, or deploy knowing the page can answer verbs the package cannot.
Publishing under the *same* version is not among the options — npm does
not allow it, and it would not be honest if it did.

Run: python3 tools/check_wasm_published.py
"""

import json
import pathlib
import re
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
WASM = ROOT / "crates/kevy-wasm/pkg/kevy.wasm"
PKG = ROOT / "crates/kevy-wasm/pkg/package.json"

# Verbs whose presence in the binary distinguishes one feature set from
# another. Chosen because each one names a whole capability the browser
# build either has or does not: they are the difference a reader would
# actually run into.
MARKERS = ["IDX.CREATE", "IDX.QUERY", "VIEW.CREATE", "TABLE.DECLARE"]


def verbs(blob: bytes) -> set:
    """Which markers this binary carries. A wasm module keeps its verb
    names as plain bytes in the data section, so this needs no engine."""
    return {m for m in MARKERS if m.encode() in blob}


def published(name: str, version: str, into: pathlib.Path):
    """The published tarball's kevy.wasm, or None if that version is not
    on the registry (the first publish of a version, and a network
    failure, are different things — this returns None only for a 404)."""
    url = f"https://registry.npmjs.org/{name}"
    with urllib.request.urlopen(url, timeout=30) as r:
        meta = json.load(r)
    entry = meta.get("versions", {}).get(version)
    if entry is None:
        return None
    tgz = into / "pkg.tgz"
    urllib.request.urlretrieve(entry["dist"]["tarball"], tgz)
    with tarfile.open(tgz) as t:
        member = next((m for m in t.getmembers() if m.name.endswith("kevy.wasm")), None)
        if member is None:
            sys.exit("check_wasm_published: the published tarball has no kevy.wasm")
        return t.extractfile(member).read()


def main():
    if not WASM.exists():
        print("check_wasm_published: no kevy.wasm — run `npm run engine` in web/")
        return 1
    meta = json.loads(PKG.read_text(encoding="utf-8"))
    name, version = meta["name"], meta["version"]

    mine = WASM.read_bytes()
    with tempfile.TemporaryDirectory() as tmp:
        theirs = published(name, version, pathlib.Path(tmp))

    if theirs is None:
        print(f"ok: {name}@{version} is not published yet — nothing to disagree with")
        return 0

    if mine == theirs:
        print(f"ok: the built wasm is byte-identical to {name}@{version} ({len(mine) // 1024} KB)")
        return 0

    mine_v, theirs_v = verbs(mine), verbs(theirs)
    print(f"check_wasm_published: the built wasm is NOT {name}@{version}\n")
    print(f"  built     {len(mine) // 1024:>5} KB   {', '.join(sorted(mine_v)) or '(none of the markers)'}")
    print(f"  published {len(theirs) // 1024:>5} KB   {', '.join(sorted(theirs_v)) or '(none of the markers)'}")

    only_here = mine_v - theirs_v
    if only_here:
        print(
            f"\n  The site would demonstrate {', '.join(sorted(only_here))}, which "
            f"{name}@{version} cannot answer.\n"
            f"  A reader who tries the terminal and then installs the package gets less\n"
            f"  than the page showed. Closing that is a publish under a NEW version, not\n"
            f"  a deploy — npm will not let you republish {version}, and it would not be\n"
            f"  honest if it would."
        )
    else:
        print("\n  Same markers, different bytes — a rebuild, or a change below the marker level.")
    print("\n  Deploy anyway only if that is a decision somebody made, not one nobody saw.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
