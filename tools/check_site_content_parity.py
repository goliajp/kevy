#!/usr/bin/env python3
"""Every word of the written pages must reach the built site.

The trilingual page content — 4,600 lines of prose in
tools/site_content/{en,zh,ja}.py — was moved to a new site by exporting it
and rendering it, not by rewriting it. This proves the move lost nothing:
every text-bearing field in every block of every page is looked for in the
HTML that page produced.

It is a text check, not a structural one, on purpose. A block that renders
in the wrong shape is a visible bug somebody will report; a paragraph that
silently stopped being rendered is not, and that is the failure mode of
every content migration.

Run: python3 tools/check_site_content_parity.py
"""

import html
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DIST = ROOT / "web/dist"
CONTENT = ROOT / "web/src/content.json"

# Fields that hold prose. Everything else in a block is structure: ids,
# hrefs, tone names, block types.
# Every field that carries something a reader is meant to see, including
# the code. Leaving `code` out is how four recipe blocks rendered with no
# commands in them while this gate still reported every fragment present:
# the prose around the commands was there, and the commands were the part
# that mattered.
TEXT_KEYS = {
    "h1", "h2", "h3", "h", "lede", "intro", "body", "title", "text", "note",
    "caption", "goal", "cost", "q", "a", "label", "summary", "eyebrow",
    "code", "do", "kicker", "go",
}


# Containers to walk INTO. Without these the recursion stops at the first
# dict whose key is not itself a text key, which is how `items` — the list
# holding every card, step, recipe line and tab — was skipped whole. The
# gate reported 423 fragments present and had never looked at one command.
CONTAINER_KEYS = {"items", "blocks", "rows", "fields"}


def strings(node, out):
    """Every prose string in a block, however deeply nested."""
    if isinstance(node, str):
        out.append(node)
    elif isinstance(node, list):
        for x in node:
            strings(x, out)
    elif isinstance(node, dict):
        for k, v in node.items():
            if k in TEXT_KEYS or k in CONTAINER_KEYS:
                strings(v, out)


# Tags the content genuinely writes by hand for emphasis and links. Only
# these are stripped — `<768 f32, little-endian>` inside a code sample is
# not markup, and treating every angle bracket as a tag deleted it from
# the source side while the page rendered it correctly, reporting sixteen
# false losses.
INLINE_TAG = re.compile(r"</?(?:b|i|em|strong|code|a|span|kbd)\b[^>]*>", re.I)
# `<br>` is not decoration around a word, it is a line: dropping it joins
# "the data" to "and the way" into one token that appears on no page.
BREAK_TAG = re.compile(r"<br\s*/?>", re.I)


def normalise(s, *, source):
    """The words a reader sees. Comparing markup would fail on formatting
    differences that change nothing.

    `source` distinguishes the two sides: the content writes a handful of
    inline tags by hand, while the rendered page is HTML throughout."""
    if source:
        s = BREAK_TAG.sub(" ", s)
        s = INLINE_TAG.sub("", s)
    else:
        s = re.sub(r"<[^>]+>", " ", s)
    s = html.unescape(s)
    # ALL whitespace goes, both sides.
    #
    # React emits a space between an element and its neighbouring text, so
    # `<b>…read</b>, and` renders as `read , and` and `(<code>EF</code>)`
    # as `( EF)`. In Japanese, where the source has no spaces at all, every
    # `</b>` boundary produces one. Chasing these with punctuation rules
    # took 160 false losses down to 128 and would never have reached zero:
    # the artefact is "whitespace appears at element boundaries", and the
    # boundaries are wherever the prose happens to be marked up.
    #
    # Whitespace is not evidence of content loss. A dropped paragraph
    # changes the characters; a moved space does not. Comparing with all of
    # it removed is what makes this gate answer the question it asks.
    return re.sub(r"\s+", "", s)


def main():
    if not DIST.exists():
        sys.exit("check_site_content_parity: no web/dist — run npm run build in web/")
    if not CONTENT.exists():
        sys.exit("check_site_content_parity: no web/src/content.json — run tools/export_site_content.py")

    data = json.loads(CONTENT.read_text(encoding="utf-8"))
    missing = []
    checked = 0
    pages = 0

    for lang, page_map in sorted(data.items()):
        for slug, page in sorted(page_map.items()):
            # The English landing page is the React app: its text lives in
            # web/src/i18n.tsx, is rendered in the browser, and verify.mjs
            # checks it there. The other two languages get a static one.
            if slug == "" and lang == "en":
                continue
            rel = (f"{slug}/index.html" if lang == "en" else f"{lang}/{slug}/index.html").replace("//", "/")
            f = DIST / rel
            if not f.exists():
                missing.append(f"{lang}:{slug or '(home)'}: no page was built at {rel}")
                continue
            pages += 1
            rendered = normalise(f.read_text(encoding="utf-8"), source=False)

            texts = []
            strings(page.get("blocks", []), texts)
            for t in texts:
                want = normalise(t, source=True)
                # Very short fragments ("OK", a single verb) collide with
                # chrome and prove nothing either way.
                if len(want) < 12:
                    continue
                checked += 1
                if want not in rendered:
                    missing.append(
                        f"{lang}:{slug or '(home)'}: text is in the content and not on the page\n"
                        f"        {want[:120]}"
                    )

    if not checked:
        sys.exit("check_site_content_parity: compared nothing — the content export is empty")

    if missing:
        print(f"check_site_content_parity: FAIL — {len(missing)} problem(s)\n")
        for m in missing[:15]:
            print(f"  ✗ {m}")
        if len(missing) > 15:
            print(f"  … and {len(missing) - 15} more")
        print("\nContent that exists in the source and not on the page is content the")
        print("migration dropped. Nothing else reports it.")
        sys.exit(1)

    print(f"ok: {checked} text fragments across {pages} pages, all present on the built site")


if __name__ == "__main__":
    main()
