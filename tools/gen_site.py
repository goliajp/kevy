#!/usr/bin/env python3
"""Render the marketing and scenario pages in every language.

Content lives in tools/site_content/<lang>.py; the HTML lives in
tools/site_render.py. A page that exists in English and not in Japanese is an
error here rather than a hole on the site, which is the only way three languages
stay in step.

Run: python3 tools/gen_site.py [--check]
"""

import importlib.util
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
from site_render import DIRS, page  # noqa: E402


def load(lang):
    p = ROOT / "tools/site_content" / f"{lang}.py"
    if not p.exists():
        return None
    spec = importlib.util.spec_from_file_location(f"content_{lang}", p)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m.PAGES


def main():
    check = "--check" in sys.argv
    en = load("en")
    want = {}
    missing = []

    for lang in ("en", "zh", "ja"):
        pages = load(lang)
        if pages is None:
            missing.append(f"{lang}: no content file yet")
            continue
        gap = set(en) - set(pages)
        if gap:
            missing.append(f"{lang}: missing {sorted(gap)}")
        # Page-level parity is not enough. A translation can carry every page and
        # still render last month's landing, because nothing checked that the
        # BLOCKS matched — and one did exactly that, silently, while every gate
        # was green. The block types and their order are the page's skeleton;
        # they have to line up.
        for slug in sorted(set(en) & set(pages)):
            skeleton = [b["t"] for b in en[slug]["blocks"]]
            got = [b["t"] for b in pages[slug]["blocks"]]
            if skeleton != got:
                missing.append(
                    f"{lang}: /{slug or ''} block drift\n"
                    f"      en: {' '.join(skeleton)}\n"
                    f"      {lang}: {' '.join(got)}"
                )
                continue
            # And INSIDE each block. Type-and-order parity is not enough: a FAQ
            # can carry the right block type and one fewer question, a table the
            # right shape and one fewer row, and every gate stays green while the
            # two languages quietly say different things. It happened — English
            # kept a question Chinese had cut, and nothing noticed.
            for i, (a, b) in enumerate(zip(en[slug]["blocks"], pages[slug]["blocks"])):
                for key in ("items", "rows", "body", "ctas", "head"):
                    # `body` is a list of paragraphs in a prose block and a
                    # single string in a callout. Only the lists are counted.
                    if not isinstance(a.get(key), list):
                        continue
                    if len(a[key]) != len(b.get(key, [])):
                        missing.append(
                            f"{lang}: /{slug or ''} block {i} ({a['t']}): "
                            f"{key} has {len(b.get(key, []))} entries, en has {len(a[key])}"
                        )
                # The markers that colour a table cell are DATA — a `!` row is
                # red in one language and must be red in all of them.
                if a["t"] == "table":
                    for r, (ra, rb) in enumerate(zip(a["rows"], b.get("rows", []))):
                        ma = [c[0] if str(c)[:1] in "*!" else "" for c in ra]
                        mb = [c[0] if str(c)[:1] in "*!" else "" for c in rb]
                        if ma != mb:
                            missing.append(
                                f"{lang}: /{slug or ''} table row {r}: "
                                f"emphasis markers differ from en"
                            )
                if a["t"] == "bars":
                    if [x[4] for x in a["rows"]] != [x[4] for x in b.get("rows", [])]:
                        missing.append(f"{lang}: /{slug or ''} bars: thin-margin flags differ from en")
        for slug, spec in pages.items():
            out = ROOT / "site" / DIRS[lang] / (slug + "/" if slug else "") / "index.html"
            want[out] = page(spec, lang, slug)

    if check:
        stale = [p for p, t in want.items()
                 if not p.exists() or p.read_text(encoding="utf-8") != t]
        if stale or missing:
            for m in missing:
                print(f"INCOMPLETE {m}")
            for p in stale[:5]:
                print(f"STALE {p.relative_to(ROOT)}")
            print("Regenerate with `python3 tools/gen_site.py`.")
            return 1
        print(f"ok: {len(want)} site pages match their content")
        return 0

    for p, t in want.items():
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(t, encoding="utf-8")
    for m in missing:
        print(f"  TODO {m}")
    print(f"  wrote {len(want)} pages")
    return 0


if __name__ == "__main__":
    sys.exit(main())
