#!/usr/bin/env python3
"""The wasm size the site quotes is the size of the wasm the site ships.

The landing page, the reference and the README all told readers the browser
build was "218 KB gzipped over the wire" in three languages and nineteen
places. It was 481 KB. The number was right when somebody measured it and
then went on being quoted for every release after, because nothing measured
it again — the same way the site said 4.0 while shipping 5.1.0.

So it is measured here, from the artifact the npm package would carry, and
compared against every place the number is written down. The tolerance is
10%: gzip output moves with the zlib version, but by a percent or two, not
by ten. It was 25% at first, and 25% was too kind — docs/wasm.md said "416
KB uncompressed" for a 1442 KB module, and 416 sits inside 25% of the 481
KB compressed figure, so the gate read a number that was wrong about a
different quantity as a number that was nearly right about this one. A
tolerance wide enough to absorb a category error is not a tolerance.

Run: python3 tools/check_wasm_size.py [--write]

  --write   rewrite the stated sizes to what was measured. Without it the
            check only reports, which is what CI runs.
"""

import gzip
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
WASM = ROOT / "crates/kevy-wasm/pkg/kevy.wasm"

# Where the size is quoted. Prose in three languages plus the two documents
# a reader reaches from GitHub.
SOURCES = [
    "tools/site_content/en.py",
    "tools/site_content/zh.py",
    "tools/site_content/ja.py",
    "README.md",
    "docs/wasm.md",
    "docs/cookbook.md",
    # The translations quote the size too, and quote it separately: while
    # English said 416 KB the Chinese and Japanese said 425 KB, a number
    # from an even older build that no gate was looking at because this
    # list stopped at the English chapter.
    "docs/zh/wasm.md",
    "docs/ja/wasm.md",
]

# A claim is "<n> KB" within a few words of something naming the browser
# build. Bare sizes elsewhere (the 655 KB IoT binary, a 4 KB page) are not
# this gate's business and must not be rewritten by it.
# No trailing group: a `[^\n]{0,80}` tail after the unit is greedy, so it
# swallowed the rest of the line and finditer only ever saw the FIRST size
# on it — "(496 KB packed, 218 KB gzipped)" reported one claim, and a stale
# second number was invisible. The context window is taken from the line
# below instead, where it cannot eat the next match.
CLAIM = re.compile(r"(?P<size>\d{2,4})\s*KB\b", re.I)
NEAR = re.compile(r"wasm|WebAssembly|gzip|gzipped|packed|回線|ブラウザ|浏览器|browser", re.I)
# The IoT/core-tier binary is quoted in the same sentences as the browser
# build ("655 KB IoT" sits on a line that also says "the browser"), and it
# is a different artifact with its own budget. Naming it excludes the claim
# rather than letting this gate rewrite a number it does not measure.
NOT_OURS = re.compile(r"IoT|no_std|chip|チップ|芯片|core tier|`core`", re.I)

TOLERANCE = 0.10


def measured():
    """(gzipped wasm KB, raw wasm KB) — what crosses the wire, and what it
    unpacks to. npm transfers the tarball gzipped, and the wasm is 97% of
    it, so the wasm's own gzip size is the honest "over the wire" number."""
    if not WASM.exists():
        return None
    raw = WASM.read_bytes()
    # mtime=0 so the answer does not depend on the clock.
    return round(len(gzip.compress(raw, 9, mtime=0)) / 1024), round(len(raw) / 1024)


def claims():
    """Every stated size that sits next to a word about the browser build."""
    out = []
    for rel in SOURCES:
        p = ROOT / rel
        if not p.exists():
            continue
        text = p.read_text(encoding="utf-8")
        for line_no, line in enumerate(text.splitlines(), 1):
            for m in CLAIM.finditer(line):
                window = line[max(0, m.start() - 60) : m.end() + 60]
                if NEAR.search(window) and not NOT_OURS.search(window):
                    out.append((rel, line_no, int(m.group("size")), line.strip()))
    return out


def main():
    write = "--write" in sys.argv
    got = measured()
    if got is None:
        print(
            f"check_wasm_size: {WASM.relative_to(ROOT)} is missing — "
            "build it with `cd web && npm run engine`"
        )
        return 1
    gz, raw = got

    found = claims()
    # A gate that finds nothing must not pass. Nineteen places quote this
    # number; if a rename or a rewrite drops the selector to a handful, the
    # silence would read as agreement.
    if len(found) < 10:
        print(f"check_wasm_size: only {len(found)} size claims found — the selector is wrong")
        return 1

    # Which claims are about the compressed size and which about the raw one
    # is decided by the sentence, not by this script guessing: a claim is
    # wrong when it is near neither measurement.
    def ok(n):
        return any(abs(n - target) <= target * TOLERANCE for target in (gz, raw))

    stale = [c for c in found if not ok(c[2])]

    if write and stale:
        for rel in {c[0] for c in stale}:
            p = ROOT / rel
            text = p.read_text(encoding="utf-8")
            for _, _, n, _ in [c for c in stale if c[0] == rel]:
                text = text.replace(f"{n} KB", f"{gz} KB")
            p.write_text(text, encoding="utf-8")
        print(f"check_wasm_size: rewrote {len(stale)} claim(s) to {gz} KB")
        return 0

    if stale:
        print(f"check_wasm_size: FAIL — {len(stale)} stated size(s) no longer true")
        print(f"  measured: {gz} KB gzipped, {raw} KB raw\n")
        for rel, line_no, n, line in stale[:8]:
            print(f"  ✗ {rel}:{line_no} says {n} KB")
            print(f"      {line[:100]}")
        print("\n  python3 tools/check_wasm_size.py --write")
        return 1

    print(
        f"ok: {len(found)} stated wasm sizes across {len({c[0] for c in found})} files, "
        f"all within {int(TOLERANCE * 100)}% of the artifact ({gz} KB gzipped, {raw} KB raw)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
