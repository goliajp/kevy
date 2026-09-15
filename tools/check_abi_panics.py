#!/usr/bin/env python3
"""abi-panic-gate — no panic may cross an ABI boundary.

Unwinding out of an `extern` function into C, Java or JavaScript is
undefined behaviour, not a crash with a good error message. Rust made
this abort by default for `extern "C"` in the 2021 edition, which turns
the UB into a process abort — better, and still not an answer for a
library whose whole job is to be embedded in someone else's process: an
abort takes their application down with no way to handle it.

So every function this workspace exports across an ABI must either
catch, or say in writing why it cannot panic. This gate holds that.

What it checks, for every `extern "<abi>" fn` DEFINITION (declarations
inside `extern { ... }` blocks are imports and are not our problem):

  * the body mentions `catch_unwind`, directly or through a helper this
    file names; or
  * a `// NO-UNWIND: <reason>` line sits immediately above it.

A reason is required, for the same reason a LOC waiver and an accepted
set-growth require one: the exemption must not outlive whoever
understood it.

Floors, because a check that finds nothing must fail rather than pass:
every crate known to export a boundary must still export one, and the
total must not fall below a recorded minimum. A refactor that renames
the pattern out of this script's sight would otherwise read as a
perfect score.

Exit: 0 clean, 1 an unguarded boundary or a floor breach.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Crates that must always have at least one exported boundary. If one of
# these reaches zero, the pattern moved and this gate stopped looking at
# anything.
FLOORS = {"kevy-ffi": 20, "kevy-jni": 15, "kevy-napi": 1}
TOTAL_FLOOR = 45

# A definition, not a declaration: `extern "abi" fn name(` with a body.
DEF = re.compile(r'^\s*(?:pub\s+)?(?:unsafe\s+)?extern\s+"([A-Za-z0-9_-]+)"\s+fn\s+([A-Za-z0-9_]+)')
WAIVER = re.compile(r"//\s*NO-UNWIND:\s*(\S.*)")
# Helpers that wrap catch_unwind. Named here rather than guessed at, so
# adding one is a deliberate act that shows up in a diff.
GUARDS = ("catch_unwind", "guard_abi", "abi_guard", "with_panic_guard")


def body_of(lines: list[str], start: int) -> str:
    """Text from the signature to its closing brace, by brace depth."""
    depth, out, seen = 0, [], False
    for line in lines[start:]:
        out.append(line)
        depth += line.count("{") - line.count("}")
        if "{" in line:
            seen = True
        if seen and depth <= 0:
            break
    return "".join(out)


def main() -> int:
    unguarded: list[str] = []
    waived: list[str] = []
    per_crate: dict[str, int] = {}
    total = 0

    for path in sorted(ROOT.glob("crates/*/src/**/*.rs")):
        rel = path.relative_to(ROOT)
        if "tests" in path.name or path.name.startswith("test"):
            continue
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines(keepends=True)
        in_extern_block = False
        depth_at_block = 0
        depth = 0
        for i, line in enumerate(lines):
            # Skip `extern "C" { ... }` — those are imports we call, not
            # boundaries we export.
            if re.search(r'extern\s+"[A-Za-z0-9_-]+"\s*\{', line):
                in_extern_block, depth_at_block = True, depth
            depth += line.count("{") - line.count("}")
            if in_extern_block and depth <= depth_at_block:
                in_extern_block = False
            if in_extern_block:
                continue
            m = DEF.match(line)
            if not m:
                continue
            abi, name = m.group(1), m.group(2)
            crate = path.relative_to(ROOT / "crates").parts[0]
            per_crate[crate] = per_crate.get(crate, 0) + 1
            total += 1
            note = None
            for back in range(max(0, i - 4), i):
                w = WAIVER.search(lines[back])
                if w:
                    note = w.group(1).strip()
            body = body_of(lines, i)
            if any(g in body for g in GUARDS):
                continue
            if note:
                waived.append(f"{rel}:{i + 1} {name} (\"{abi}\") — {note}")
                continue
            unguarded.append(f"{rel}:{i + 1} {name} (\"{abi}\")")

    print(f"abi-panic-gate — {total} exported ABI boundaries across {len(per_crate)} crates")
    for crate in sorted(per_crate):
        print(f"  {per_crate[crate]:>4}  {crate}")

    bad = False
    if total < TOTAL_FLOOR:
        print(f"\nFAIL floor: {total} boundaries found, at least {TOTAL_FLOOR} expected — "
              "the pattern moved and this gate stopped seeing them")
        bad = True
    for crate, floor in sorted(FLOORS.items()):
        got = per_crate.get(crate, 0)
        if got < floor:
            print(f"FAIL floor: {crate} exports {got} boundaries, at least {floor} expected")
            bad = True

    if waived:
        print(f"\n{len(waived)} waived with a stated reason:")
        for w in waived:
            print(f"  {w}")

    if unguarded:
        print(f"\nFAIL {len(unguarded)} boundary/boundaries can unwind into a foreign caller:")
        for u in unguarded:
            print(f"  {u}")
        print("\nWrap the body in catch_unwind, or state why it cannot panic with a")
        print("`// NO-UNWIND: <reason>` line directly above the function.")
        return 1

    if bad:
        return 1
    print("\nabi-panic-gate: PASS — every exported boundary catches or says why it need not")
    return 0


if __name__ == "__main__":
    sys.exit(main())
