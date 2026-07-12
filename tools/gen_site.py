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
