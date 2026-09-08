#!/usr/bin/env python3
"""The tree, cut small enough to judge one piece at a time.

L2 review asks "what would best practice be for this, and what is it?"
That question has an answer only at a size one reader can hold: a crate
of 25,000 lines does not have a best-practice shape, and `kevy-rt` as a
whole cannot be compared against anything. So the tree is cut by
responsibility until each unit is one subject.

The cut is derived, not written down: a directory under `src/` is a
unit, and so is a `name_*.rs` family in a flat crate. `kevy-rt` has
`uring_*` (16 files), `exec_*` (23), `shard_*` (5) — those are the
units, and the crate is only their sum.

Units are sized so one reviewer can read the whole thing: under about
2,000 lines. Anything larger is marked SPLIT and cut again before it is
sent anywhere.
"""

import collections
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
READABLE = 2000  # lines one reviewer can hold at once


def is_test(p):
    """Test files carry a review of their own; they are not the subject
    here. The first version matched `tests_*.rs` and `*_tests.rs` and
    missed `store_tests_more.rs`, which is neither — so match the token
    wherever it sits."""
    n = p.name
    return n == "tests.rs" or "tests" in re.split(r"[_.]", n)


def units():
    stones = {s["crate"] for s in json.loads((ROOT / "bench/STONE-REPORT.json").read_text())["stones"]}
    out = []
    for crate_dir in sorted((ROOT / "crates").iterdir()):
        src = crate_dir / "src"
        if not src.is_dir():
            continue
        files = [p for p in src.rglob("*.rs") if not is_test(p)]
        if not files:
            continue
        groups = collections.defaultdict(list)
        for p in files:
            rel = p.relative_to(src)
            key = rel.parts[0] if len(rel.parts) > 1 else re.split(r"[_.]", rel.name)[0]
            groups[key].append((str(rel), len(p.read_text(errors="replace").splitlines())))
        crate_loc = sum(l for v in groups.values() for _, l in v)
        # A crate small enough to read whole is one unit, not many.
        if crate_loc <= READABLE:
            out.append({
                "crate": crate_dir.name, "unit": "(whole crate)",
                "stone": crate_dir.name in stones,
                "files": len(files), "loc": crate_loc,
                "members": [n for n, _ in sorted(files and groups and
                            [(n, l) for v in groups.values() for n, l in v], key=lambda x: -x[1])],
            })
            continue
        for key, v in groups.items():
            out.extend(cut(crate_dir.name, key, v, crate_dir.name in stones))
    return out


def cut(crate, key, members, stone, depth=0):
    """One unit if it can be read whole; otherwise cut again.

    The second cut is by the next token of the filename — `exec_op.rs`
    and `exec_dispatch.rs` are `exec/op` and `exec/dispatch` — and the
    third and fourth go on down the path. A family still too big after
    that is a single file over 2,000 lines, which locgate already
    refuses, so the recursion terminates on a tree this gate keeps.
    """
    loc = sum(l for _, l in members)
    if loc <= READABLE or depth >= 3 or len(members) == 1:
        return [{
            "crate": crate, "unit": key, "stone": stone,
            "files": len(members), "loc": loc,
            "members": [n for n, _ in sorted(members, key=lambda x: -x[1])],
        }]
    sub = collections.defaultdict(list)
    for name, l in members:
        stem = pathlib.Path(name).stem
        parts = re.split(r"[_/]", name.replace(".rs", ""))
        token = parts[depth + 1] if len(parts) > depth + 1 else stem
        sub[f"{key}/{token}"].append((name, l))
    out = []
    for k, v in sub.items():
        out.extend(cut(crate, k, v, stone, depth + 1))
    return out


def main() -> int:
    us = units()
    us.sort(key=lambda u: (-u["loc"]))
    ready = [u for u in us if u["loc"] <= READABLE]
    split = [u for u in us if u["loc"] > READABLE]

    lines = ["# The tree, cut into units one reviewer can hold", "",
             f"{len(us)} units across {len({u['crate'] for u in us})} crates, "
             f"{sum(u['loc'] for u in us):,} lines.", "",
             f"**{len(ready)} are ready to review** (≤ {READABLE} lines). "
             f"**{len(split)} need cutting again** before they are sent anywhere — "
             "a unit nobody can read whole gets a review nobody should trust.", ""]

    if split:
        lines += ["## Cut these further", "",
                  "| crate | unit | files | lines |", "|---|---|---:|---:|"]
        for u in split:
            lines.append(f"| {u['crate']} | `{u['unit']}` | {u['files']} | {u['loc']} |")
        lines.append("")

    lines += ["## Ready to review", "",
              "| crate | unit | | files | lines |", "|---|---|---|---:|---:|"]
    for u in ready:
        lines.append(f"| {u['crate']} | `{u['unit']}` | {'stone' if u['stone'] else ''} "
                     f"| {u['files']} | {u['loc']} |")

    (ROOT / "quality/DECOMPOSITION.md").write_text("\n".join(lines) + "\n")
    print(f"decompose: {len(us)} units, {len(ready)} readable, {len(split)} need cutting")
    for u in split:
        print(f"  SPLIT  {u['crate']}/{u['unit']:22s} {u['loc']:6d} lines, {u['files']} files")
    print(f"\n-> quality/DECOMPOSITION.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
