#!/usr/bin/env python3
"""The compatibility headline is a measurement, not a sentence.

`README.md` says N commands are reply-checked byte-for-byte against
valkey 9.1, and so do the Japanese and Chinese READMEs and
`docs/UPGRADING.md`. That number came from a documentation rewrite and
nothing has held it since: the corpus in `bench/compat3.sh` had 81
distinct verbs while all four documents said 98.

It is the strongest compatibility claim this project makes — the only
one measured against a real valkey and a real redis rather than against
kevy's own second implementation — and a headline nobody can check is
worth less than no headline at all.

So the number is derived from the corpus, and every document that
states it must state the same one.

Run: python3 tools/check_compat_claim.py
Exit: 0 agree, 1 disagree, 2 refused (the read is broken).
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Each document, and the pattern that finds the number in its own
# language. The count is the same; the sentence around it is not.
CLAIMS = [
    ("README.md", re.compile(r"(\d+) commands\b")),
    ("README.ja.md", re.compile(r"(\d+)個の\s*\n?\s*コマンド")),
    ("README.zh-CN.md", re.compile(r"(\d+) 条命令")),
    ("docs/UPGRADING.md", re.compile(r"\((\d+) commands\)")),
]


def corpus_verbs() -> set[str]:
    """Distinct verbs the three-way differential actually drives."""
    text = (ROOT / "bench/compat3.sh").read_text()
    return {
        m.group(2).upper()
        for m in re.finditer(r"^\s*(check|checku)\s+(\S+)", text, re.M)
    }


def main() -> int:
    verbs = corpus_verbs()
    # A floor. A parse that found nothing would make every document
    # agree with zero, which is the direction that hides a broken read.
    if len(verbs) < 50:
        print(
            f"check_compat_claim: REFUSED — the corpus parse found {len(verbs)} "
            "verbs; that is a broken read, not a shrunken corpus",
            file=sys.stderr,
        )
        return 2
    want = len(verbs)

    bad = []
    for name, pattern in CLAIMS:
        path = ROOT / name
        if not path.exists():
            print(f"check_compat_claim: REFUSED — {name} is missing", file=sys.stderr)
            return 2
        found = [int(m) for m in pattern.findall(path.read_text())]
        if not found:
            print(
                f"check_compat_claim: REFUSED — no compatibility count found in "
                f"{name}; the claim moved and this check went blind",
                file=sys.stderr,
            )
            return 2
        for n in found:
            if n != want:
                bad.append((name, n))

    if bad:
        print(f"check_compat_claim: FAIL — bench/compat3.sh drives {want} verbs")
        for name, n in bad:
            print(f"  {name} says {n}")
        print("  Update the documents, or the corpus, so they answer the same question.")
        return 1

    print(
        f"check_compat_claim: ok — {want} verbs in the three-way differential, "
        f"and all {len(CLAIMS)} documents say so"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
