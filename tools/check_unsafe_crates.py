#!/usr/bin/env python3
"""The set of crates that contain unsafe code does not grow unnoticed.

A crate counts when a non-test source file under its src/ has `unsafe {`,
`unsafe fn`, `unsafe impl`, `unsafe extern` or `unsafe trait`. The allowed
set is bench/.unsafe-crates-baseline; adding a crate there is a deliberate
act with its reason in the commit. Exit 1 names any crate outside the set.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE = ROOT / "bench" / ".unsafe-crates-baseline"
UNSAFE = re.compile(r"(^|[^_A-Za-z0-9])unsafe\s*(\{|fn |impl |extern |trait )", re.M)
TEST_FILE = re.compile(r"/(tests?|abi_tests|[a-z_]*_tests)\.rs$")


def crates_with_unsafe():
    out = set()
    for src in sorted((ROOT / "crates").glob("*/src")):
        for f in src.rglob("*.rs"):
            if TEST_FILE.search(str(f)):
                continue
            if UNSAFE.search(f.read_text(encoding="utf-8", errors="replace")):
                out.add(src.parent.name)
                break
    return out


def main():
    allowed = {line.strip() for line in BASELINE.read_text().splitlines()
               if line.strip() and not line.startswith("#")}
    actual = crates_with_unsafe()
    extra = sorted(actual - allowed)
    if extra:
        print(f"unsafe appeared in a crate outside bench/.unsafe-crates-baseline: {' '.join(extra)}")
        return 1
    print(f"{len(actual)} crates carry unsafe, none outside the recorded set")
    return 0


if __name__ == "__main__":
    sys.exit(main())
