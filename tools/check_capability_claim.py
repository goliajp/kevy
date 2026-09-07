#!/usr/bin/env python3
"""The command count the site publishes is the count the engine answers.

`check_compat_claim.py` guards the differential headline ("N commands
reply-checked byte-for-byte"). This guards the other published number: how
many commands kevy implements at all. They are different claims and only one
of them had a judge.

The site said **188 commands** in three languages while `COMMAND COUNT`
answered 205 and the page that sentence links to listed 205 — for long enough
that nobody knows when it drifted. The alignment RFC's layer 4 covers
published *measurements* (the benchmark ratios, kept by
`sync_readme_bench.py`); a capability claim is not a measurement and fell
between the two.

Source of truth: the rows of `crates/kevy/src/verb_meta/`, which is what
`COMMAND`, `COMMAND COUNT`, llms.txt and the MCP schema all answer from.

Run: python3 tools/check_capability_claim.py
Exit: 0 agree, 1 disagree, 2 refused (the read is broken).
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
VERB_META = ROOT / "crates" / "kevy" / "src" / "verb_meta"

# Where the number is stated, and the pattern that finds it in that file's
# own language. A pattern that finds nothing is a broken read, not agreement.
CLAIMS = [
    ("tools/site_content/en.py", re.compile(r"(\d+)\s+commands\b")),
    ("tools/site_content/zh.py", re.compile(r"(\d+)\s*条命令")),
    # The Japanese counter is written as a character class so this line is not
    # itself CJK prose with ASCII punctuation in it — check_cjk_punct reads
    # every file, including its siblings.
    ("tools/site_content/ja.py", re.compile("(\\d+)\\s*(?:" + "\u500b\u306e" + ")?\\s*"
                                            + "\u30b3\u30de\u30f3\u30c9")),
]


def implemented() -> int:
    rows = 0
    for path in sorted(VERB_META.glob("*.rs")):
        rows += len(re.findall(r'^    v\("[A-Z][A-Z0-9._|-]*"', path.read_text(), re.M))
    return rows


def main() -> int:
    want = implemented()
    if want < 100:
        print(f"check_capability_claim: REFUSED — parsed {want} verbs from VERB_META; "
              "that is a broken read, not a shrunken surface", file=sys.stderr)
        return 2

    bad = []
    for name, pattern in CLAIMS:
        path = ROOT / name
        if not path.exists():
            print(f"check_capability_claim: REFUSED — {name} is missing", file=sys.stderr)
            return 2
        found = [int(m if isinstance(m, str) else m[0]) for m in pattern.findall(path.read_text())]
        if not found:
            print(f"check_capability_claim: REFUSED — no command count found in {name}; "
                  "the claim moved and this check went blind", file=sys.stderr)
            return 2
        for n in found:
            if n != want:
                bad.append(f"{name} says {n} commands, the engine answers {want}")

    # The site's own command reference must list them all, or the sentence
    # links to a page that disagrees with it.
    ref = ROOT / "web" / "src" / "commands.json"
    if ref.exists():
        import json
        data = json.loads(ref.read_text())
        listed = len(data if isinstance(data, list) else data.get("commands", data))
        if listed != want:
            bad.append(f"web/src/commands.json lists {listed}, the engine answers {want}")

    for line in bad:
        print(f"  {line}", file=sys.stderr)
    print(f"check_capability_claim: {'FAIL' if bad else 'ok'} — {want} verbs in VERB_META, "
          f"{len(CLAIMS) + 1} statements checked")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
