#!/usr/bin/env python3
"""Refuse half-width punctuation inside Chinese and Japanese prose.

A `,` between two Chinese characters is not a typo you notice once and fix. It
is the kind of thing that comes back every time someone edits the page, because
the keyboard makes the wrong character the easy one. So it gets a gate rather
than a code review.

Chinese takes 「,」 for a clause break and 「、」 to separate list items.
Japanese takes 「、」 for both. Neither takes ASCII `,` `.` `;` `:` `!` `?`
adjacent to CJK text. Inside code, `<pre>`, `<code>`, attributes, URLs and
Latin-script runs, ASCII punctuation is correct and is left alone.

Run: python3 tools/check_cjk_punct.py            # checks site/zh, site/ja, docs
     python3 tools/check_cjk_punct.py <paths…>
"""

import pathlib
import re
import sys

CJK = r"぀-ヿ㐀-䶿一-鿿豈-﫿"
BAD = ",.;:!?"

# ASCII punctuation with CJK on either side of it.
#
# Two shapes are NOT violations and must not be flagged:
#   * a digit on either side — `3.5`, `1,000`, and markdown's `### 2. レプリカ`,
#     where the dot is an ordinal marker rather than a full stop;
#   * a Latin letter after the mark — `kevy, Redis` inside an English fragment.
PAT = re.compile(
    rf"(?:[{CJK}][{re.escape(BAD)}](?![\d\w])"
    rf"|(?<![\d])[{re.escape(BAD)}][ \t]*[{CJK}])"
)

# Regions where ASCII punctuation is the correct character: code, markup,
# attributes, URLs, and entities.
STRIP = [
    re.compile(r"<pre\b.*?</pre>", re.S | re.I),
    re.compile(r"<code\b.*?</code>", re.S | re.I),
    re.compile(r"<script\b.*?</script>", re.S | re.I),
    re.compile(r"<style\b.*?</style>", re.S | re.I),
    re.compile(r"```.*?```", re.S),  # markdown fences
    re.compile(r"`[^`\n]+`"),  # inline markdown code
    re.compile(r"<[^>]+>"),  # tags, with all their attributes
    re.compile(r"https?://\S+"),
    re.compile(r"&[a-z]+;"),
]


def blank(text):
    """Blank out the code/markup regions, keeping every offset AND every line.

    Newlines have to survive: the caller zips this against the raw text
    line-by-line, and a multi-line <pre> collapsed into spaces would shift every
    line number after it.
    """
    def spaces(m):
        return "".join("\n" if ch == "\n" else " " for ch in m.group(0))

    for pat in STRIP:
        text = pat.sub(spaces, text)
    return text


def check(path):
    raw = path.read_text(encoding="utf-8")
    hits = []
    for lineno, (src, stripped) in enumerate(
        zip(raw.splitlines(), blank(raw).splitlines()), 1
    ):
        for m in PAT.finditer(stripped):
            lo = max(0, m.start() - 24)
            hits.append((lineno, m.group(0).strip(), src[lo : m.end() + 24].strip()))
    return hits


def main():
    args = sys.argv[1:]
    root = pathlib.Path(__file__).resolve().parent.parent
    if args:
        targets = [pathlib.Path(a) for a in args]
    else:
        targets = [root / "site/zh", root / "site/ja", root / "docs/zh", root / "docs/ja"]

    files = []
    for t in targets:
        if t.is_dir():
            files += sorted(t.rglob("*.html")) + sorted(t.rglob("*.md"))
        elif t.exists():
            files.append(t)

    total = 0
    for f in files:
        hits = check(f)
        if not hits:
            continue
        total += len(hits)
        try:
            rel = f.resolve().relative_to(root)
        except ValueError:
            rel = f  # a path outside the repo, e.g. a one-off check
        for lineno, mark, ctx in hits[:6]:
            print(f"{rel}:{lineno}: half-width {mark!r} in CJK prose — {ctx}")
        if len(hits) > 6:
            print(f"{rel}: … and {len(hits) - 6} more")

    if total:
        print()
        print(f"REFUSED: {total} half-width punctuation marks in CJK prose "
              f"across {len(files)} files.")
        print("Chinese: 「,」 for a clause, 「、」 between list items. "
              "Japanese: 「、」 for both. Never ASCII ',' next to CJK.")
        return 1
    print(f"ok: {len(files)} Chinese/Japanese files, no half-width punctuation in prose")
    return 0


if __name__ == "__main__":
    sys.exit(main())
