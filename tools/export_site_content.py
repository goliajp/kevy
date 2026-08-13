#!/usr/bin/env python3
"""Export the trilingual page content to JSON for the website build.

The content lives in tools/site_content/{en,zh,ja}.py — 4,600 lines of
written, translated prose that took real work and must survive the move to
the new site unchanged. It is exported rather than rewritten: a hand
migration of that much text is a migration that quietly loses some of it.

The exporter is deliberately dumb. It reads the Python dicts and writes
them out verbatim; it does not interpret a single field. The web build then
renders each block type, and check_site_content_parity.py compares what
came out against what went in, character by character, so nothing can be
dropped in transit.

Run: python3 tools/export_site_content.py [--check]
"""

import importlib.util
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "web/src/content.json"
LANGS = ("en", "zh", "ja")


def load(lang):
    p = ROOT / "tools/site_content" / f"{lang}.py"
    if not p.exists():
        sys.exit(f"export_site_content: no content for {lang}")
    spec = importlib.util.spec_from_file_location(f"content_{lang}", p)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m.PAGES


def main():
    check = "--check" in sys.argv
    data = {}
    for lang in LANGS:
        pages = load(lang)
        data[lang] = pages

    # Every language must carry every page. A page that exists in English
    # and not in Japanese is a hole in the site, and the previous generator
    # treated it as an error for exactly that reason.
    en_slugs = set(data["en"])
    for lang in LANGS[1:]:
        missing = en_slugs - set(data[lang])
        extra = set(data[lang]) - en_slugs
        if missing:
            sys.exit(f"export_site_content: {lang} is missing {sorted(missing)}")
        if extra:
            sys.exit(f"export_site_content: {lang} has pages English does not: {sorted(extra)}")

    if not en_slugs:
        sys.exit("export_site_content: no pages at all — the content module is empty")

    text = json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True) + "\n"

    if check:
        if not OUT.exists():
            sys.exit(f"export_site_content: {OUT.relative_to(ROOT)} has not been generated")
        if OUT.read_text(encoding="utf-8") != text:
            sys.exit(
                f"export_site_content: {OUT.relative_to(ROOT)} is stale — "
                "run python3 tools/export_site_content.py"
            )
        print(f"ok: {len(en_slugs)} pages x {len(LANGS)} languages, export is current")
        return

    OUT.write_text(text, encoding="utf-8")
    print(f"wrote {OUT.relative_to(ROOT)}: {len(en_slugs)} pages x {len(LANGS)} languages")


if __name__ == "__main__":
    main()
