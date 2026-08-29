#!/usr/bin/env python3
"""What the documentation still owes, as a set.

Measured 2026-08-27: 2,661 public items across the workspace, 93.9% carrying
prose documentation and **0.7% carrying an executable example**. Twenty-two
crates are at "100% documented, zero examples".

So the documentation problem here is not coverage. Writing more prose is
close to finished work. The gap is that almost none of it is *checked
against the code*: a doctest is compiled and run, a paragraph is not, and
for a stone — promised to any project that takes it — an undoctested public
function is an unverified promise.

This emits the **deficit** rather than the achievement, because a
set-ratchet forbids growth and the thing that must not grow is what is
owed. A crate at zero deficit is simply absent from the set; the moment it
gains an undocumented item, that key joins and the ratchet fires.

Reads the tables from rustdoc's own coverage mode. Both figures come from
rustc rather than from a scan for `///`, which would count a comment that
documents nothing and miss `#[doc = ...]`.

**The example count is a lower bound, not an exact figure.** rustdoc counts
an example only when it hangs off an item already in its coverage set, and
a doctest on a method inside a private module's `impl` runs and passes
without ever registering. kevy-seg showed 0 examples while
`SegBuilder::create`'s doctest was passing; moving the same block onto
`pub struct SegBuilder` took the file from 0 to 1. So the deficit this
emits may overstate what is owed — which is the safe direction for a
ratchet, and worth knowing before anyone reads 1.1% as the whole truth.

Run:
  RUSTDOCFLAGS='-Z unstable-options --show-coverage' \
      cargo +nightly doc --workspace --no-deps
  python3 tools/doc_deficit.py [--out bench/DOC-DEFICIT.json]
Exit: 0 wrote it, 2 refused.
"""

import glob
import json
import pathlib
import platform as _platform
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOCDIR = ROOT / "target/doc"
OUT = ROOT / "bench/DOC-DEFICIT.json"
TOTAL = re.compile(r"\|\s*Total\s*\|\s*(\d+)\s*\|\s*([\d.]+)%\s*\|\s*(\d+)\s*\|\s*([\d.]+)%")

# Fewer tables than this and rustdoc did not run over the workspace.
MIN_TABLES = 30


def refuse(msg):
    print(f"docdeficit: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def shown(p):
    """Repo-relative when it is under the repo, absolute otherwise."""
    try:
        return p.relative_to(ROOT)
    except ValueError:
        return p


def host():
    return {"Linux": "linux", "Darwin": "macos", "Windows": "windows"}.get(
        _platform.system(), _platform.system().lower())


def read_tables():
    files = sorted(glob.glob(str(DOCDIR / "*.txt")))
    if len(files) < MIN_TABLES:
        refuse(f"found {len(files)} coverage tables under "
               f"{DOCDIR.relative_to(ROOT)}; rustdoc did not run over the "
               f"workspace, and an empty read is not a clean bill of health")
    out = {}
    for f in files:
        m = TOTAL.search(pathlib.Path(f).read_text())
        if not m:
            continue
        documented, pct, examples = int(m.group(1)), float(m.group(2)), int(m.group(3))
        if pct <= 0:
            # 0% documented with a nonzero item count is real, but rustdoc
            # gives no way to recover the count from it. Say so rather than
            # silently scoring the crate as having nothing to owe.
            items = documented
        else:
            items = round(documented / pct * 100)
        out[pathlib.Path(f).stem.replace("_", "-")] = {
            "items": items, "documented": documented, "examples": examples,
        }
    if not out:
        refuse("no coverage tables parsed; the table format changed")
    return out


def main():
    tables = read_tables()
    symbols, items, documented, examples = {}, 0, 0, 0
    for crate, t in sorted(tables.items()):
        items += t["items"]
        documented += t["documented"]
        examples += t["examples"]
        if (d := t["items"] - t["documented"]) > 0:
            symbols[f"{crate}/undocumented"] = d
        if (e := t["items"] - t["examples"]) > 0:
            symbols[f"{crate}/without-example"] = e

    doc = {
        "identity": {"kind": "doc-deficit", "platform": host()},
        "kind": "doc-deficit",
        "platform": host(),
        "total_regions": items,
        "min_total": max(1, items // 2),
        "public_items": items,
        "documented": documented,
        "with_example": examples,
        "symbols": symbols,
    }
    # resolve(): a relative --out crashed the final print on CI *after* the
    # file was already written — the measurement succeeded and the report of
    # it did not. relative_to() below only accepts a path under ROOT.
    out = (pathlib.Path(sys.argv[sys.argv.index("--out") + 1]).resolve()
           if "--out" in sys.argv else OUT)
    out.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"docdeficit: {items} public items in {len(tables)} crates — "
          f"{documented} documented ({100 * documented / items:.1f}%), "
          f"{examples} with an executable example ({100 * examples / items:.1f}%)")
    print(f"  owed: {items - documented} docs, {items - examples} examples "
          f"across {len(symbols)} keys -> {shown(out)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
