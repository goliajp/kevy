#!/usr/bin/env python3
"""Refuse a dead internal link anywhere in site/.

A dead link on a documentation site is worse than a missing page: the reader
trusted you enough to click, and you spent that trust on a 404. The command
reference alone is 555 generated pages with relative paths four levels deep —
exactly the shape where an off-by-one in the generator goes unnoticed until
someone else finds it. (It did. Every English command page pointed one level too
high and lost its own stylesheet.)

External links are not checked: the network is not a build dependency, and a
gate that fails because someone else's server is down is a gate people learn to
ignore.

Run: python3 tools/check_links.py
"""

import pathlib
import re
import sys
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent
SITE = ROOT / "site"

SKIP = ("http://", "https://", "#", "mailto:", "data:", "javascript:")


def main():
    bad = {}
    n_links = 0
    pages = [f for f in sorted(SITE.rglob("*.html")) if "demo/pkg" not in str(f)]

    for f in pages:
        text = f.read_text(encoding="utf-8")
        for attr in ("href", "src"):
            for raw in re.findall(rf'{attr}="([^"]+)"', text):
                if raw.startswith(SKIP):
                    continue
                n_links += 1
                # Strip a query or fragment before resolving.
                path = urllib.parse.urlsplit(raw).path
                if not path:
                    continue
                if path.startswith("/"):
                    target = SITE / path.lstrip("/")
                else:
                    target = (f.parent / path).resolve()
                # A directory link resolves to its index.html.
                ok = target.exists() or (target / "index.html").exists()
                if not ok:
                    bad.setdefault(raw, []).append(f.relative_to(ROOT))

    for raw, srcs in sorted(bad.items()):
        eg = srcs[0]
        more = f" (+{len(srcs) - 1} more)" if len(srcs) > 1 else ""
        print(f"dead: {raw}\n      from {eg}{more}")

    if bad:
        print()
        print(f"REFUSED: {len(bad)} dead internal links across {len(pages)} pages.")
        return 1
    print(f"ok: {n_links} internal links, none dead")
    return 0


if __name__ == "__main__":
    sys.exit(main())
