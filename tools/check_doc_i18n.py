#!/usr/bin/env python3
"""Refuse a translation that quietly fell behind its English chapter.

The docs are published in three languages, and nothing noticed when one
of them stopped keeping up. On 2026-08-06 `docs/zh/tables.md` still
carried a line reading "this page is not translated yet, see the
English version" for a feature that had shipped, and four other
chapters were each missing a whole section — including the one section
in `views.md` that exists to tell a reader they may not want a view at
all. Every one of those was written in English and never carried over,
and no gate could see it: link checking, punctuation and site
generation all pass on a chapter that is half there.

So this asks the only question that is mechanical: **does each
translation have the same number of level-2 sections as its English
original?** It cannot tell you a section was translated *well*. It can
tell you one is missing, which is the failure that actually happened.

    python3 tools/check_doc_i18n.py

A chapter that is deliberately English-only belongs in ENGLISH_ONLY
with the reason written down, not left to fail silently.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"

# Deliberately English-only, each for a stated reason. These are not
# chapters a reader is handed: they are records addressed to one
# audience at one moment, and translating them would be inventing a
# readership they never had.
ENGLISH_ONLY = {
    "DEFECT-REPORT-2026-07-20-ATOMIC-ERROR-PATH-RESPONSE.md": "a dated defect report",
    "REPORT-FROM-GOLIAJP-2026-07-20-EMBEDDED-AS-PRIMARY-STORE.md": "a consumer's report",
    "REPORT-RESPONSE-2026-07-20-EMBEDDED-AS-PRIMARY-STORE.md": "the reply to it",
    "SUPPORT-LINE-3X-VS-4X-2026-07-20.md": "a dated support statement",
    "client-contract.md": "the contract client authors implement, in the language they file issues in",
    "clients.md": "a list of client packages and their install lines",
    "electron.md": "not translated yet — tracked here rather than passing silently",
    "tauri.md": "not translated yet — tracked here rather than passing silently",
    "verb-reference.md": "generated verb table, not prose",
}

HEADING = re.compile(r"^## (.+?)\s*$", re.M)
FENCE = re.compile(r"^(?:```|~~~).*?^(?:```|~~~)", re.M | re.S)


def sections(path):
    """Level-2 headings outside fenced blocks (a `## ` inside a shell
    transcript is output, not a section)."""
    text = FENCE.sub("", path.read_text(encoding="utf-8"))
    return HEADING.findall(text)


def main():
    problems = []
    checked = 0
    for en in sorted(DOCS.glob("*.md")):
        name = en.name
        if name in ENGLISH_ONLY:
            continue
        want = sections(en)
        for lang in ("zh", "ja"):
            other = DOCS / lang / name
            if not other.exists():
                problems.append(f"{lang}/{name}: missing entirely ({len(want)} sections in English)")
                continue
            checked += 1
            got = sections(other)
            if len(got) != len(want):
                problems.append(
                    f"{lang}/{name}: {len(got)} sections, English has {len(want)}"
                )
    if problems:
        print(f"REFUSED: {len(problems)} translation(s) out of step with the English chapter.")
        for p in problems:
            print(f"  {p}")
        print("  Translate the missing section, or add the file to ENGLISH_ONLY")
        print("  in tools/check_doc_i18n.py with the reason.")
        return 1
    print(f"ok: {checked} translated chapters, each with its English chapter's sections")
    return 0


if __name__ == "__main__":
    sys.exit(main())
