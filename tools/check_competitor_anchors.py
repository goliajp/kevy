#!/usr/bin/env python3
"""Every competitor kevy measures itself against is pinned, and current.

A comparison carries a version whether or not anybody writes it down. The
2026-09-01 arena table published seven ratios against "Redis 8" — the
script named the image by bare major, docker served the 8.10.0 layer it
had cached in August, and the registry by then served 8.10.1. Neither number
reached the ledger. The same floating tag would have served 8.0 a year
earlier, and 8.0 loses to 8.10 by a wide margin on the very verbs the
table reports, so the published ratio was a function of the box's docker
cache. That is the defect this gate closes.

Three ways to fail, and the third is the one that only an outward call
can catch:

  MISSING  — a site in the anchors file matches nothing. A gate that
             reads the tree can only ever prove the tree agrees with
             itself; one whose pattern has rotted proves nothing at all
             and must say so rather than pass.
  DRIFT    — a pin in the tree disagrees with COMPETITOR-ANCHORS.json.
  STALE    — the anchors file itself is behind the upstream's latest
             stable release. This is the question no amount of reading
             our own files can answer, so the gate asks GitHub, Docker
             Hub and endoflife.date directly, and an upstream it cannot
             reach is a failure, never a pass.

Run: python3 tools/check_competitor_anchors.py [--offline] [--json]
"""

import fnmatch
import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
ANCHORS = ROOT / "bench" / "COMPETITOR-ANCHORS.json"
TIMEOUT = 20


PLAIN_VERSION = re.compile(r"^\d+(\.\d+){0,3}$")


def semver(v: str) -> tuple:
    """Compare only versions that are plainly numeric.

    This read every run of digits as a component, so "8.10.0-rc1" became
    (8,10,0,1) and sorted ABOVE (8,10,0) — a prerelease reading as newer
    than the release it precedes. Anything that is not purely numeric now
    refuses to be compared rather than being compared wrongly."""
    if not PLAIN_VERSION.match(v):
        raise ValueError(f"not a plain version: {v!r}")
    parts = [int(p) for p in v.split(".")]
    return tuple(parts + [0] * (4 - len(parts)))


def fetch(url: str) -> object:
    req = urllib.request.Request(url, headers={"User-Agent": "kevy-anchor-gate"})
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token and "api.github.com" in url:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
        return json.loads(r.read().decode())


def latest_github(spec: dict) -> str:
    """Newest non-prerelease tag. Redis and Valkey both maintain several
    release lines at once (8.10.1 and 8.8.2 land the same day), so this
    takes the largest version rather than the most recent publication."""
    prefix = spec.get("tag_prefix", "")
    rels = fetch(f"https://api.github.com/repos/{spec['repo']}/releases?per_page=60")
    best = ""
    for rel in rels:
        if rel.get("prerelease") or rel.get("draft"):
            continue
        tag = rel["tag_name"]
        if prefix and not tag.startswith(prefix):
            continue
        tag = tag[len(prefix):]
        if not re.fullmatch(r"\d+\.\d+(\.\d+)?", tag):
            continue  # milestones (8.12-m01) and rcs are not stable
        if not best or semver(tag) > semver(best):
            best = tag
    if not best:
        raise LookupError(f"no stable release among {len(rels)} from {spec['repo']}")
    return best


def latest_endoflife(spec: dict) -> str:
    cycles = fetch(f"https://endoflife.date/api/{spec['product']}.json")
    best = ""
    for c in cycles:
        v = c.get("latest") or ""
        if re.fullmatch(r"\d+\.\d+(\.\d+)?", v) and (not best or semver(v) > semver(best)):
            best = v
    if not best:
        raise LookupError(f"no release in endoflife feed for {spec['product']}")
    return best


def latest_dockerhub(spec: dict) -> str:
    url = f"https://hub.docker.com/v2/repositories/{spec['repo']}/tags?page_size=100"
    tags = [t["name"] for t in fetch(url)["results"]]
    vers = [t for t in tags if re.fullmatch(r"\d+\.\d+\.\d+(-v\d+)?", t)]
    if not vers:
        raise LookupError(f"no versioned tag among {len(tags)} on {spec['repo']}")
    return max(vers, key=semver)


RESOLVERS = {"github": latest_github, "endoflife": latest_endoflife, "dockerhub": latest_dockerhub}


MANIFEST_REF = "COMPETITOR-ANCHORS.json"


def check_site(name: str, site: dict, pinned: str) -> list:
    """One site, one verdict list. Two modes: a file that carries a literal
    tag must carry the pinned one; a file that reads this manifest at runtime
    must actually read it and must not also carry a literal."""
    path = ROOT / site["file"]
    where = f"{name}: {site['file']}"
    if not path.exists():
        return [f"MISSING  {where} does not exist"]
    text = path.read_text(encoding="utf-8")
    found = re.findall(site["pattern"], text)
    if site.get("mode") == "derive":
        if MANIFEST_REF not in text:
            return [f"MISSING  {where} is declared derive but never reads {MANIFEST_REF}"]
        return [f"DRIFT    {where} derives its pin yet also hardcodes {v}" for v in sorted(set(found))]
    if not found:
        return [f"MISSING  {where}: pattern matched nothing"]
    return [f"DRIFT    {where} pins {v}, anchors file says {pinned}"
            for v in sorted(set(found)) if v != pinned]


# Any reference to a competitor image, PINNED OR NOT. The previous pattern
# required `:[0-9v]`, which meant it saw exactly the references that were
# already fine and missed `redis:latest`, a bare `redis`, a digest pin and
# `redis:${TAG}` — the floating forms this whole file exists to eliminate.
IMAGE_NAMES = (r"redis/redis-stack-server", r"redis/redis-stack", r"valkey/valkey",
               r"dragonflydb/dragonfly", r"pgvector/pgvector", r"redis", r"valkey", r"postgres")
_NAMES = "|".join(IMAGE_NAMES)

# Two rules, because one was wrong in both directions. Requiring a numeric
# tag saw only references that were already fine and missed `redis:latest`,
# a bare `redis` and `redis@sha256:…` — the floating forms this file exists
# to remove. Accepting a bare name anywhere then matched the word "redis" in
# prose, in a log, and in node_modules' sqlite3.c.
#
#   TAGGED  — a name carrying any tag or digest, anywhere. `:latest`,
#             `:${VAR}` and `@sha256:` are the point, not the exception.
#   IN_CTX  — a bare name, but only where docker is being told to run it.
# The tag has to look like a tag. Accepting any word after the colon made
# `for cfg in "redis:start_redis"` — a shell label:function pair — read as an
# image reference. Versions, the known floating names, and a shell/CI variable
# are what a tag actually is here.
_TAG_SHAPE = r"(?:v?[0-9][A-Za-z0-9._-]*|latest|edge|unstable|alpine[A-Za-z0-9._-]*|" \
             r"bookworm|trixie|bullseye|slim[A-Za-z0-9._-]*|\$\{[A-Za-z_][A-Za-z0-9_]*\})"
TAGGED_RE = re.compile(r"\b(" + _NAMES + r")(:" + _TAG_SHAPE + r"|@sha256:[0-9a-f]+)")
IN_CTX_RE = re.compile(
    r"(?:docker\s+run[^\n]*?|image:\s*[\"']?|FROM\s+|--entrypoint\s+\S+\s+)"
    r"\b(" + _NAMES + r")\b(?![:/.-])")
IMAGE_PATTERNS = (TAGGED_RE, IN_CTX_RE)

# Build artefacts, vendored trees and recorded output are not the live
# surface: a competitor name in a .log or a .sql dump is a record, not a run.
SWEEP_SKIP_SUFFIX = (".md", ".log", ".txt", ".test", ".sql", ".json.gz")
SWEEP_SKIP_PATH = ("__pycache__", "node_modules", "/target/", "/dist/", "/.build/")
SWEPT = ("bench", "tools", "examples", ".github")


def unregistered(data: dict) -> list:
    """Competitor images in the live surface that no anchor claims.

    Every other check in this file starts from the anchors list, so a bench
    script added next year that pulls its own redis image would be invisible
    to all of them — the gate would stay green while a new, unwatched
    version quietly entered the measurements."""
    # D7: per-competitor. A file that is a site for redis was invisible to
    # the sweep for postgres too, which is the case unregistered() exists for.
    claimed = {name: {s["file"] for s in a["sites"]} for name, a in data["anchors"].items()}
    every_site = set().union(*claimed.values()) if claimed else set()
    exempt = tuple(data["_exempt"])
    out = []
    for top in SWEPT:
        for path in sorted((ROOT / top).rglob("*")):
            rel = str(path.relative_to(ROOT))
            if (not path.is_file() or rel.endswith(SWEEP_SKIP_SUFFIX)
                    or any(s in rel for s in SWEEP_SKIP_PATH)):
                continue
            # fnmatch, not rstrip("*") — the glob in the key is a glob.
            if any(fnmatch.fnmatch(rel, e) or rel == e for e in exempt):
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            for hit in [h for pat in IMAGE_PATTERNS for h in pat.finditer(text)]:
                image = hit.group(1)
                owner = next((n for n, a in data["anchors"].items()
                              if a.get("image", "").split(":")[0].split("/")[-1] == image.split("/")[-1]), None)
                if owner and rel in claimed.get(owner, ()):
                    continue          # this file is a registered site FOR THIS competitor
                if not owner and rel in every_site:
                    continue          # an image no anchor owns, in a file some anchor watches
                out.append(f"UNREGISTERED {rel} names {hit.group(0).strip()} "
                           f"but is in no anchor's site list")
                break
    return out


def check_anchor(name: str, anchor: dict, offline: bool) -> dict:
    pinned = anchor["pinned"]
    fails = [f for site in anchor["sites"] for f in check_site(name, site, pinned)]
    upstream, note = None, ""
    if not offline:
        try:
            upstream = RESOLVERS[anchor["upstream"]["kind"]](anchor["upstream"])
        except (urllib.error.URLError, LookupError, KeyError, OSError, ValueError) as e:
            fails.append(f"UNREACHABLE {name}: cannot read upstream ({e.__class__.__name__}: {e})")
        else:
            if semver(upstream) > semver(pinned):
                fails.append(f"STALE    {name}: upstream stable is {upstream}, we pin {pinned}")
            elif semver(upstream) < semver(pinned):
                fails.append(f"UNCONFIRMED {name}: we pin {pinned}, ahead of upstream's "
                             f"latest stable {upstream} — no upstream can confirm this version")
    return {"name": name, "pinned": pinned, "upstream": upstream, "note": note,
            "sites": len(anchor["sites"]), "manual": anchor.get("manual", False), "fails": fails}


def main() -> int:
    offline = "--offline" in sys.argv
    data = json.loads(ANCHORS.read_text(encoding="utf-8"))
    results = [check_anchor(n, a, offline) for n, a in data["anchors"].items()]
    sweep = unregistered(data)
    if sweep:
        results.append({"name": "(sweep)", "pinned": "-", "upstream": None, "note": "",
                        "sites": 0, "manual": False, "fails": sweep})
    if "--json" in sys.argv:
        print(json.dumps(results, indent=2))
    else:
        print(f"competitor anchors — {ANCHORS.relative_to(ROOT)}"
              + ("  [OFFLINE: staleness NOT checked]" if offline else ""))
        for r in results:
            mark = "FAIL" if r["fails"] else "ok  "
            up = r["upstream"] or ("?" if offline else "-")
            tail = "  MANUAL (no pin site)" if r["manual"] else f"  {r['sites']} site(s)"
            print(f"  {mark} {r['name']:<12} pinned {r['pinned']:<10} upstream {up:<10}{tail} {r['note']}")
    fails = [f for r in results for f in r["fails"]]
    for f in fails:
        print(f"  {f}", file=sys.stderr)
    print(f"\n{len(fails)} problem(s) across {len(results)} anchors")
    if offline:
        # Exit 2, not 0. This printed "it is not a pass" and then returned a
        # pass, so a caller that added --offline to the online row would have
        # silently downgraded the gate to tree-agrees-with-itself.
        print("  --offline skipped the upstream question: exit 2, for a caller that "
              "accepts a tree-only answer.", file=sys.stderr)
        return 1 if fails else 2
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
