#!/usr/bin/env python3
"""Every fuzz target in the tree runs in CI, or this fails.

A fuzz target that exists and is never run is worse than one that does not
exist: the repository shows the coverage and the coverage is not there.
Nine of twenty-three were in that state when this gate was written —
including `kevy-compress/decode_arbitrary` and `kevy-seg/seg_open`, the two
targets whose whole job is to be fed arbitrary bytes, and which are exactly
what would have caught the unbounded reservation found by hand in
`kevy-seg`'s footer decoder the same day.

The matrix in `.github/workflows/ci.yml` is hand-written, so it drifts. This
reconciles it against the filesystem, both ways:

- **A target with no matrix entry fails.** That is the drift this exists for.
- **A matrix entry with no target fails too.** A job that fuzzes something
  deleted is a job that will one day fail for a reason nobody can find.

Floor rule: finding no targets, or parsing no matrix entries, is a broken
producer and not a pass. The instrument must be able to fail before its
silence means anything.

Run: python3 tools/check_fuzz_matrix.py
Exit: 0 pass, 1 violation, 2 refused.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CI = ROOT / ".github/workflows/ci.yml"


def refuse(msg):
    print(f"fuzzgate: REFUSED — {msg}")
    sys.exit(2)


def targets_in_tree():
    return {
        (f.parts[-4], f.stem)
        for f in ROOT.glob("crates/*/fuzz/fuzz_targets/*.rs")
    }


def targets_in_matrix():
    if not CI.exists():
        refuse(f"no workflow at {CI.relative_to(ROOT)}")
    block = re.search(
        r"fuzz-smoke:.*?include:\n(.*?)\n\s*steps:", CI.read_text(), re.S
    )
    if not block:
        refuse("the fuzz-smoke matrix did not parse — an unparsed matrix is "
               "not an empty one, and must not read as agreement")
    return set(re.findall(r"crate:\s*([\w-]+),\s*target:\s*([\w-]+)", block.group(1)))


def main():
    tree = targets_in_tree()
    matrix = targets_in_matrix()
    if not tree:
        refuse("no fuzz targets found under crates/*/fuzz/fuzz_targets — "
               "the producer failed, this is not a pass")
    if not matrix:
        refuse("the matrix parsed to zero entries — see above")

    unrun = sorted(tree - matrix)
    phantom = sorted(matrix - tree)
    if unrun or phantom:
        print("fuzzgate: FAIL")
        for c, t in unrun:
            print(f"  ✗ {c}/{t} exists and never runs — add it to the "
                  f"fuzz-smoke matrix")
        for c, t in phantom:
            print(f"  ✗ the matrix runs {c}/{t}, which is not in the tree")
        return 1

    print(f"fuzzgate: PASS — {len(tree)} fuzz targets, all of them in the matrix")
    return 0


if __name__ == "__main__":
    sys.exit(main())
