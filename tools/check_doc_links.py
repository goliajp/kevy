#!/usr/bin/env python3
"""Refuse a dead link — file *or* anchor — between the markdown docs.

`check_links.py` guards the rendered site. Nothing guarded the sources,
so the two faces of a subject could drift apart silently: this gate was
written the day `docs/rds-workloads.md` was found refusing an `OFFSET`
that `docs/tables.md` documented and the engine executed. A cross
reference is the one mechanical part of "these two pages agree", and it
is worth having a machine hold it.

Anchors are checked, not just paths — a link that lands on the right
page at the wrong place is the failure a reader actually feels, and it
is the one that survives a rename. Slugs follow GitHub's rule
(lowercase, drop punctuation, spaces to hyphens, `-1` on collision),
because that is what renders these files.

Run: python3 tools/check_doc_links.py
"""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# What this gate is for: the docs a reader is handed. `site/` is
# generated (check_links.py holds it) and untracked build output is not
# ours to fix. `.claude/` is excluded on purpose — private working notes
# and dated audit records, some of which cite documents that were
# deliberately deleted; freezing their paths would be rewriting a record
# to please a linter.
SKIP_DIRS = ("site/", ".claude/")
SKIP_LINKS = ("http://", "https://", "mailto:", "data:", "javascript:", "tel:")

# [text](target) — inline links only. Reference-style links and bare
# autolinks carry no relative paths in this repo.
LINK = re.compile(r"(?<!\\)\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*$", re.M)
FENCE = re.compile(r"^(?:```|~~~).*?^(?:```|~~~)", re.M | re.S)


def slug(text):
    """GitHub's heading slug: the anchor a reader's click actually uses."""
    # Strip inline markup that never reaches the anchor.
    text = re.sub(r"`([^`]*)`", r"\1", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"[*_]{1,3}([^*_]+)[*_]{1,3}", r"\1", text)
    text = text.strip().lower()
    text = re.sub(r"[^\w\s-]", "", text, flags=re.UNICODE)
    # Each space becomes one hyphen — `A / B` drops the slash and keeps
    # both spaces, so the anchor is `a--b`. Collapsing them here would
    # have this gate reject links that render fine.
    return re.sub(r"\s", "-", text)


def anchors_of(text):
    """Every anchor a page offers: its headings, plus explicit ids."""
    body = FENCE.sub("", text)
    out, seen = set(), {}
    for _, title in HEADING.findall(body):
        s = slug(title)
        if not s:
            continue
        n = seen.get(s, 0)
        seen[s] = n + 1
        out.add(s if n == 0 else f"{s}-{n}")
    out.update(re.findall(r'(?:id|name)="([^"]+)"', body))
    return out


def main():
    # Tracked files only: generated packages (`examples/*/pkg/`) ship a
    # README written by a tool, against paths that exist in the package
    # and not in this tree.
    tracked = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "*.md"],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    files = [
        ROOT / p for p in sorted(tracked) if not any(p.startswith(s) for s in SKIP_DIRS)
    ]
    anchors = {f: anchors_of(f.read_text(encoding="utf-8")) for f in files}

    bad, n_links = [], 0
    for f in files:
        body = FENCE.sub("", f.read_text(encoding="utf-8"))
        for raw in LINK.findall(body):
            if raw.startswith(SKIP_LINKS):
                continue
            path, _, frag = raw.partition("#")
            n_links += 1
            # A link whose TARGET is under `.claude/` (private working
            # notes, intentionally gitignored) or absolute (a machine-
            # specific path) is an external reference, not a repo-internal
            # doc link — skip it the same way the source dirs are skipped.
            if path.startswith("/") or ".claude/" in path:
                continue
            target = f if not path else (f.parent / path).resolve()
            # Also skip if it resolved to somewhere outside the repo root
            # (e.g. a `../` chain escaping into a sibling project).
            try:
                target.relative_to(ROOT)
            except ValueError:
                continue
            if not target.exists():
                bad.append((f, raw, "no such file"))
                continue
            if not frag or target.suffix != ".md":
                continue
            known = anchors.get(target)
            if known is None:  # outside the scanned set — path check only
                continue
            if frag not in known:
                bad.append((f, raw, "no such heading"))

    for f, raw, why in bad:
        print(f"dead ({why}): {raw}\n      from {f.relative_to(ROOT)}")

    if bad:
        print()
        print(f"REFUSED: {len(bad)} dead doc links across {len(files)} files.")
        return 1
    print(f"ok: {n_links} markdown links across {len(files)} files, none dead")
    return 0


if __name__ == "__main__":
    sys.exit(main())
