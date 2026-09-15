#!/usr/bin/env python3
"""`expect` must give the reason it cannot fail, not describe the failure.

The rulebook allows `.expect(...)` in library code exactly where a type or
a preceding check makes the failure impossible, and it is specific about
the string: it has to be the REASON, not the error. That half cannot be
linted — clippy counts `expect` calls and has no view on what they say —
so it went unchecked, and the tree filled up with strings that answer a
different question:

    page[0..2].try_into().expect("2 bytes")        a length
    tr[0..8].try_into().expect("8")                a length
    Layout::array::<u8>(cap).expect("drop layout") a label
    self.get(key).expect("no entry found for key") the error

None of those tell a reader why the call is sound, which is the only
thing that would let them change the surrounding code safely.

What this decides is narrow, and it was narrowed on evidence. A first
version also rejected any string containing a failure word, and that
flagged `expect("tier: vlog read failed — per-boot spill file, this is a
process bug")`, whose clause after the dash is exactly the reason, along
with `expect("kraft overflow implies a deepenable symbol")` and two
reasons written the same hour as the check. A gate that is red on correct
code stops being read, so it now decides two things and no more:

  * the string is a bare measurement — `"4"`, `"8 bytes"`
  * the string states the conclusion rather than the premise —
    "cannot fail", "never fails", "unreachable"

Whether `expect("the len >= 8 arm")` is TRUE is checkable by reading, and
this tool does not read. It closes the classes that are definitely not a
reason and leaves the rest where it belongs.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# A call's string, however it is spelled across lines.
EXPECT = re.compile(r'\.expect\(\s*"((?:[^"\\]|\\.)*)"', re.S)

BARE_MEASURE = re.compile(r"^\d+(\s*(bytes?|b|bits?|chars?|elements?))?$", re.I)

# Fixed phrases, not bare words. "never" alone rejected `expect("frame is
# never empty")`, which is a premise and a good one; the first version of
# this file flagged it, along with two reasons written the same hour it
# was written — including `expect("a capacity that overflows a Layout
# could not have been allocated")`, caught by its own "overflow" rule.
#
# A gate that is red on correct code stops being read. Everything that
# needed an argument was cut, and what is left decides two things only.
STATES_A_CONCLUSION = re.compile(
    r"(should ?n[o']t fail|must ?n[o']t fail|cannot fail|can'?t fail|"
    r"never fails?|won'?t fail|is impossible|unreachable|can'?t happen)",
    re.I,
)


def is_test(p: pathlib.Path) -> bool:
    s = str(p)
    return (
        "/tests/" in s
        or "/benches/" in s
        or "/examples/" in s
        or "/fuzz/" in s
        or p.name == "tests.rs"
        or p.name.endswith("_tests.rs")
    )


def judge(msg: str):
    if BARE_MEASURE.match(msg.strip()):
        return "a measurement, not a reason"
    if STATES_A_CONCLUSION.search(msg):
        return "states the conclusion; the premise is what a reader needs"
    return None


def main() -> int:
    bad, seen = [], 0
    for p in sorted((ROOT / "crates").rglob("*.rs")):
        if is_test(p):
            continue
        text = p.read_text(encoding="utf-8", errors="replace")
        for m in EXPECT.finditer(text):
            seen += 1
            why = judge(m.group(1))
            if why:
                line = text.count("\n", 0, m.start()) + 1
                bad.append((p.relative_to(ROOT), line, m.group(1), why))

    # The floor. A regex that stops matching finds nothing, and nothing is
    # exactly what a clean tree looks like from here.
    if seen < 100:
        print(f"FAIL: only {seen} expect() calls found in library code; "
              f"the tree has far more, so this run did not read it")
        return 1

    for rel, line, msg, why in bad:
        print(f"  {rel}:{line}\n      expect({msg!r}) — {why}")
    verb = "is" if len(bad) == 1 else "are"
    print(f"\n{seen} expect() calls in library code, {len(bad)} of which {verb} "
          f"not a reason")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
