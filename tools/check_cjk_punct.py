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
#     where the dot is an ordinal marker rather than a full stop.
#
# A Latin letter after the mark is NOT an excuse. 「注意:replica」 is exactly as
# wrong as 「注意:レプリカ」 — the colon belongs to the Chinese clause that opened
# it, not to the English word that happens to follow. An earlier version of this
# gate let those through, and a native-speaker pass found them.
# The digit exemption applies to the DOT only. `3.5` and markdown's `### 2. レ`
# are legitimate; `4.0:什么会坏` is not — a colon after a version number is still
# a Chinese colon, and it still has to be full-width. An earlier version of this
# gate excused every mark that followed a digit and let that one through.
PAT = re.compile(
    rf"(?:[{CJK}][{re.escape(BAD)}](?![\d])"
    rf"|(?<![\d])\.[ \t]*[{CJK}]"
    rf"|[,;:!?][ \t]*[{CJK}])"
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
    # Filled with \x01, not spaces. A tag is a HARD boundary: the full stop that
    # ends an English blurb inside <p>…</p> is not adjacent to the Chinese
    # heading in the <h3> that follows it, even though only markup separates
    # them in the byte stream. Filling with spaces made the matcher walk right
    # across the tag and report a violation that was not there.
    def spaces(m):
        return "".join("\n" if ch == "\n" else "\x01" for ch in m.group(0))

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
        # The generators and the playground's script carry Chinese and Japanese
        # string tables of their own. Checking only the rendered HTML would
        # catch the mistake one step downstream of where it is fixable — and
        # would miss it entirely in a string that is not rendered on every page.
        targets = [
            root / "site/zh", root / "site/ja",
            root / "docs/zh", root / "docs/ja",
            root / "site/assets", root / "site/data", root / "tools",
        ]

    files = []
    for t in targets:
        if t.is_dir():
            for ext in ("*.html", "*.md", "*.js", "*.py", "*.json"):
                files += sorted(t.rglob(ext))
        elif t.exists():
            files.append(t)
    files = sorted(set(files))

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
