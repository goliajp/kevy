#!/usr/bin/env python3
"""The READMEs' benchmark tables come from PERFORMANCE.md, not from memory.

Three READMEs carry the same two tables — kevy against valkey, and kevy's
lead over each of four engines. They were written by hand from a v4-era
measurement and were still showing it under a 5.1.0 release: GET 7.24 M/s
where the current measurement says 7.37, and a ratio against Redis 8 that
had moved with it.

This reads the `Key-value throughput` table in PERFORMANCE.md and
rewrites those rows in all three files. Nothing here invents a number, and
a row that table does not cover is left alone rather than extrapolated —
the pub/sub and embedded rows come from other harnesses.

Run: python3 tools/sync_readme_bench.py [--check]
"""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
RESULTS = ROOT / "PERFORMANCE.md"
ANCHORS = ROOT / "bench/COMPETITOR-ANCHORS.json"


def pins():
    """Which version of each competitor the READMEs are allowed to name.

    These labels used to be spelled out in this file, hardcoded to whatever
    was current when it was written — a fourth place a competitor version
    lived, and the one that writes it into three READMEs and the site. bench/COMPETITOR-ANCHORS.json is the
    only place a competitor version is written down now."""
    import json
    return {k: v["pinned"] for k, v in json.loads(
        ANCHORS.read_text(encoding="utf-8"))["anchors"].items()}
READMES = ["README.md", "README.zh-CN.md", "README.ja.md"]


def latest_arena():
    """The key-value throughput table in PERFORMANCE.md, as {verb: {engine: n}}."""
    text = RESULTS.read_text(encoding="utf-8")
    blocks = re.findall(
        r"## Key-value throughput — (\d{4}-\d{2}-\d{2}) — kevy ([\d.]+)\n.*?\n\n(\| verb.*?)\n\n",
        text,
        re.S,
    )
    if len(blocks) != 1:
        sys.exit(f"sync_readme_bench: expected one key-value throughput table in "
                 f"PERFORMANCE.md, found {len(blocks)}")
    date, version, table = blocks[0]
    rows = {}
    for line in table.split("\n")[2:]:
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 5:
            continue
        verb = cells[0]
        try:
            rows[verb] = {
                "kevy": int(cells[1].replace(",", "")),
                "redis8": int(cells[2].replace(",", "")),
                "valkey": int(cells[3].replace(",", "")),
                "dragonfly": int(cells[4].replace(",", "")),
            }
        except ValueError:
            # Say which table and which cell, rather than raising
            # `invalid literal for int(): 'encoding'` from a stack trace and
            # leaving the reader to find out that an entry titled like a
            # release re-measurement was in fact an A/B with a different
            # table under it. That is exactly what happened on 2026-08-30.
            sys.exit(
                f"sync_readme_bench: the key-value throughput table "
                f"({date}, kevy {version}) has a row this cannot read:\n"
                f"  {line.strip()}\n"
                f"Its columns are verb, kevy, redis, valkey, dragonfly, and "
                f"a ratio."
            )
    if not rows:
        sys.exit("sync_readme_bench: the arena table parsed to nothing")
    return date, version, rows


def m(n):
    return f"{n / 1e6:.2f} M/s"


def build(date, version, rows):
    """The two tables, and the sentence that dates them."""
    get, setv = rows["GET"], rows["SET"]
    pin = pins()
    head = {
        "README.md": ("Workload", "Ratio"),
        "README.zh-CN.md": ("负载", "倍数"),
        "README.ja.md": ("ワークロード", "倍率"),
    }
    lead = {
        "README.md": ("Engine", "kevy's lead"),
        "README.zh-CN.md": ("引擎", "kevy 领先"),
        "README.ja.md": ("エンジン", "kevy の優位"),
    }
    out = {}
    for f in READMES:
        a, b = head[f]
        c, d = lead[f]
        out[f] = {
            "vs": (
                f"| {a} | kevy | valkey {pin['valkey']} | {b} |\n"
                f"|---|---:|---:|---|\n"
                f"| `GET -c 50 -P 16` | {m(get['kevy'])} | {m(get['valkey'])} | "
                f"**{get['kevy'] / get['valkey']:.2f}×** |\n"
                f"| `SET -c 50 -P 16` | {m(setv['kevy'])} | {m(setv['valkey'])} | "
                f"**{setv['kevy'] / setv['valkey']:.2f}×** |"
            ),
            "lead": (
                f"| {c} | {d} |\n"
                f"|---|---:|\n"
                f"| valkey {pin['valkey']} | **{get['kevy'] / get['valkey']:.2f}×** |\n"
                f"| redis {pin['redis']} | **{get['kevy'] / get['redis8']:.2f}×** |\n"
                f"| dragonfly {pin['dragonfly']} | **{get['kevy'] / get['dragonfly']:.2f}×** |"
            ),
            "rate": m(get["kevy"]),
        }
    return out



# ── the site quotes the same run, in five more places ────────────────────
#
# The three READMEs were tied to PERFORMANCE.md; the site was not. Its
# four-engine table lives in tools/site_content/{en,zh,ja}.py, and the
# landing page carries a shorter table, a hero figure and a sentence of
# prose with the ratios written into it — all typed. After the 5.2.0
# re-measurement every one of them still said 5.1.0's numbers, and
# nothing anywhere would have said so.

SITE_CONTENT = ["tools/site_content/en.py", "tools/site_content/zh.py",
                "tools/site_content/ja.py"]
APP = "web/src/App.tsx"
I18N = "web/src/i18n.tsx"


def _m(n):
    """7,421,434 -> '7.42 M' — the landing page's shorter form."""
    return f"{n / 1_000_000:.2f} M"


def write_site(rows, version, check):
    """rows: {verb: {engine: int}}. Returns a list of complaints."""
    bad = []
    pin = pins()
    order = ["GET", "SET", "INCR", "SADD", "HSET", "LPUSH", "ZADD"]

    def headings(text):
        """The four-engine table's heading names each opponent, and a name
        without a version describes every release that ever bore it: the
        site said "Redis 8" for a table measured against one particular
        8.x. Rewritten inside the heading row only, so the prose around it
        — which argues about margins and needs a human — is left alone."""
        def one_row(m):
            row = m.group(0)
            row = re.sub(r"Redis [0-9][0-9.]*", f"Redis {pin['redis']}", row)
            row = re.sub(r"valkey [0-9][0-9.]*", f"valkey {pin['valkey']}", row)
            row = re.sub(r"Dragonfly( [0-9][0-9.]*)?", f"Dragonfly {pin['dragonfly']}", row)
            return row
        return re.sub(r'"head": \[[^\]]*\]', one_row, text)

    for rel in SITE_CONTENT:
        p = ROOT / rel
        text = p.read_text(encoding="utf-8")
        before = text
        for verb in order:
            r = rows[verb]
            ratio = r["kevy"] / r["redis8"]
            mark = "!" if ratio < 1.2 else "*"
            new = (f'["{verb}", "{r["kevy"]:,}", "{r["redis8"]:,}", '
                   f'"{r["valkey"]:,}", "{r["dragonfly"]:,}", "{mark}{ratio:.2f}×"]')
            text = re.sub(rf'\["{verb}", "[\d,]+", "[\d,]+", "[\d,]+", "[\d,]+", "[!*][\d.]+×"\]',
                          new.replace("\\", "\\\\"), text)
        text = re.sub(r'"kevy \d+\.\d+\.\d+"', f'"kevy {version}"', text)
        text = headings(text)
        if text != before:
            if check:
                bad.append(f"{rel} does not carry the {version} numbers")
            else:
                p.write_text(text, encoding="utf-8")

    # the landing page's four-row table and its hero figure
    p = ROOT / APP
    text = p.read_text(encoding="utf-8")
    before = text
    for verb in ["GET", "SET", "INCR", "HSET"]:
        r = rows[verb]
        new = (f"{{ op: '{verb}', kevy: '{_m(r['kevy'])}', "
               f"valkey: '{_m(r['valkey'])}', ratio: '{r['kevy'] / r['valkey']:.2f}×' }}")
        text = re.sub(rf"\{{ op: '{verb}', kevy: '[^']+', valkey: '[^']+', ratio: '[^']+' \}}",
                      new.replace("\\", "\\\\"), text)
    set_ratio = f"{rows['SET']['kevy'] / rows['SET']['valkey']:.2f}×"
    text = re.sub(r'<div className="v">[\d.]+×</div>', f'<div className="v">{set_ratio}</div>', text)
    if text != before:
        if check:
            bad.append(f"{APP} does not carry the {version} numbers")
        else:
            p.write_text(text, encoding="utf-8")

    # the abstract, which states both ratios to one decimal in three languages
    p = ROOT / I18N
    text = p.read_text(encoding="utf-8")
    g = rows["GET"]["kevy"] / rows["GET"]["valkey"]
    st = rows["SET"]["kevy"] / rows["SET"]["valkey"]
    new_text = re.sub(r"[\d.]+× on GET, [\d.]+× on SET", f"{g:.1f}× on GET, {st:.1f}× on SET", text)
    new_text = re.sub(r"GET 快 [\d.]+ 倍、SET 快 [\d.]+ 倍", f"GET 快 {g:.1f} 倍、SET 快 {st:.1f} 倍", new_text)
    new_text = re.sub(r"GET は [\d.]+ 倍、SET は [\d.]+ 倍", f"GET は {g:.1f} 倍、SET は {st:.1f} 倍", new_text)
    if new_text != text:
        if check:
            bad.append(f"{I18N} does not carry the {version} ratios")
        else:
            p.write_text(new_text, encoding="utf-8")
    return bad


def export_site_json(check):
    """Regenerate web/src/content.json from the site_content sources.

    `write_site` edits `tools/site_content/{en,zh,ja}.py`, but the site
    BUILDS from `web/src/content.json`, which is generated from them. On
    2026-09-01 this tool reported "3 READMEs and the site carry the
    2026-09-01 numbers" while content.json still held the previous run's,
    and the site was deployed from it — a page headed 6.2.2 quoting 6.2.0
    figures. Nothing this tool printed was false; it just stopped one step
    short of the artifact anyone reads, and said "the site" anyway.

    So the export is part of the sync, not a thing to remember afterwards.
    In --check mode a stale content.json is a stale site, reported here
    rather than only by the separate content-export gate, because this is
    the tool a person runs when they want the numbers to be current.
    """
    cmd = [sys.executable, str(ROOT / "tools/export_site_content.py")]
    if check:
        cmd.append("--check")
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT)
    if r.returncode == 0:
        return []
    if check:
        return ["web/src/content.json (run tools/export_site_content.py)"]
    sys.exit(
        "sync_readme_bench: the READMEs and site sources were rewritten, "
        "but exporting web/src/content.json failed:\n"
        f"{r.stdout}{r.stderr}"
    )


def patterns(name, t, date, version):
    """(what, regex, replacement) for one README; each must match once."""
    rows = "\n".join(t["vs"].split("\n")[2:])
    lead = "\n".join(t["lead"].split("\n")[2:]).replace("\\", "\\\\")
    ver = r"[\d.]+"
    common = [
        # matched on their own two rows so this cannot land on the lead table
        ("kevy-vs-valkey rows",
         r"\| `GET -c 50 -P 16` \|[^\n]*\n\| `SET -c 50 -P 16` \|[^\n]*", rows),
        ("four-engine lead table",
         rf"\| valkey {ver} \| \*\*[\d.]+×\*\* \|\n\| redis {ver} \| \*\*[\d.]+×\*\* \|\n"
         rf"\| dragonfly {ver} \| \*\*[\d.]+×\*\* \|", lead),
    ]
    rate = t["rate"]
    own = {
        "README.md": [
            ("dated sentence", r"re-measured \d{4}-\d{2}-\d{2} \(kevy [\d.]+\)",
             f"re-measured {date} (kevy {version})"),
            ("quoted rate", r"(kevy at\s*)[\d.]+ M/s( against each)", rf"\g<1>{rate}\g<2>"),
        ],
        "README.zh-CN.md": [
            ("dated sentence", r"\d{4}-\d{2}-\d{2} 重测（kevy [\d.]+）",
             f"{date} 重测（kevy {version}）"),
            ("quoted rate", r"(kevy\s*以 )[\d.]+ M/s", rf"\g<1>{rate}"),
        ],
        "README.ja.md": [
            ("dated sentence", r"を\d{4}-\d{2}-\d{2}に再測定した値（kevy [\d.]+）",
             f"を{date}に再測定した値（kevy {version}）"),
            ("quoted rate", r"(kevyは)[\d.]+ M/s(で)", rf"\g<1>{rate}\g<2>"),
        ],
    }
    return common + own[name]


def main():
    check = "--check" in sys.argv
    date, version, rows = latest_arena()
    tables = build(date, version, rows)
    stale = []

    for name in READMES:
        p = ROOT / name
        s = p.read_text(encoding="utf-8")
        before = s
        t = tables[name]

        # Every pattern must land exactly once. A pattern that matches
        # nothing rewrites nothing, and the check then reports the stale
        # text as current: the lead table kept an older run's ratios beside
        # the 6.3.0 rows that way, once its engine labels gained full
        # versions and stopped matching.
        for what, pat, rep in patterns(name, t, date, version):
            s, n = re.subn(pat, rep, s)
            if n != 1:
                sys.exit(f"sync_readme_bench: {name}: the {what} pattern matched "
                         f"{n} times, not once — the prose moved; update patterns()")

        if s != before:
            if check:
                stale.append(name)
            else:
                p.write_text(s, encoding="utf-8")

    stale += write_site(rows, version, check)
    stale += export_site_json(check)

    if check:
        if stale:
            print(f"sync_readme_bench: STALE — {', '.join(stale)}")
            print(f"  PERFORMANCE.md's key-value table is {date} (kevy {version}).")
            print("  Regenerate with: python3 tools/sync_readme_bench.py")
            sys.exit(1)
        print(f"ok: 3 READMEs and the site carry the {date} arena numbers (kevy {version})")
        return

    print(f"wrote the {date} arena numbers (kevy {version}) into 3 READMEs, "
          "the site sources, and web/src/content.json (what the site builds from)")


if __name__ == "__main__":
    main()
