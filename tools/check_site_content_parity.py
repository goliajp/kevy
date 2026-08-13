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
TEXT_KEYS = {
    "h1", "h2", "h3", "h", "lede", "intro", "body", "title", "text", "note",
    "caption", "goal", "cost", "q", "a", "label", "summary", "eyebrow",
}


def strings(node, out):
    """Every prose string in a block, however deeply nested."""
    if isinstance(node, str):
        out.append(node)
    elif isinstance(node, list):
        for x in node:
            strings(x, out)
    elif isinstance(node, dict):
        for k, v in node.items():
            if k in TEXT_KEYS:
                strings(v, out)


def normalise(s):
    """HTML the content wrote by hand, plus the entities the renderer adds,
    reduced to the words a reader sees. Comparing markup would fail on
    formatting differences that change nothing."""
    s = re.sub(r"<[^>]+>", "", s)
    s = html.unescape(s)
    return re.sub(r"\s+", " ", s).strip()


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
            rendered = normalise(f.read_text(encoding="utf-8"))

            texts = []
            strings(page.get("blocks", []), texts)
            for t in texts:
                want = normalise(t)
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
