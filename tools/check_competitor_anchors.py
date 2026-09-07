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


def semver(v: str) -> tuple:
    return tuple(int(p) for p in re.findall(r"\d+", v)[:4])


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


IMAGE_RE = re.compile(r"\b(redis|valkey/valkey|dragonflydb/dragonfly|postgres):[0-9v]")
SWEPT = ("bench", "tools", "examples", ".github")


def unregistered(data: dict) -> list:
    """Competitor images in the live surface that no anchor claims.

    Every other check in this file starts from the anchors list, so a bench
    script added next year that pulls its own redis image would be invisible
    to all of them — the gate would stay green while a new, unwatched
    version quietly entered the measurements."""
    known = {s["file"] for a in data["anchors"].values() for s in a["sites"]}
    exempt = tuple(k.rstrip("*") for k in data["_exempt"])
    out = []
    for top in SWEPT:
        for path in sorted((ROOT / top).rglob("*")):
            rel = str(path.relative_to(ROOT))
            if not path.is_file() or rel in known or rel.endswith(".md"):
                continue
            if any(rel.startswith(e) for e in exempt) or "/v125-" in rel or "__pycache__" in rel:
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            hit = IMAGE_RE.search(text)
            if hit:
                out.append(f"UNREGISTERED {rel} names {hit.group(0)} but is in no anchor's site list")
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
                note = f"(ahead of upstream {upstream} — a pin from a prerelease?)"
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
    if offline:
        print("  NOTE: --offline skips the upstream question entirely; it is not a pass.", file=sys.stderr)
    print(f"\n{len(fails)} problem(s) across {len(results)} anchors")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
