#!/usr/bin/env python3
"""The READMEs' benchmark tables come from the ledger, not from memory.

Three READMEs carry the same two tables — kevy against valkey, and kevy's
lead over each of four engines. They were written by hand from a v4-era
measurement and were still showing it under a 5.1.0 release: GET 7.24 M/s
where the current measurement says 7.37, and a ratio against Redis 8 that
had moved with it.

This reads the most recent `arena bare face` entry in
bench/PERF-LEDGER.md and rewrites those rows in all three files. Nothing
here invents a number, and a row the ledger does not cover is left alone
rather than extrapolated — the pub/sub and embedded rows come from other
harnesses and are not in an arena run.

Run: python3 tools/sync_readme_bench.py [--check]
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LEDGER = ROOT / "bench/PERF-LEDGER.md"
READMES = ["README.md", "README.zh-CN.md", "README.ja.md"]


def latest_arena():
    """The most recent arena table in the ledger, as {verb: {engine: n}}."""
    text = LEDGER.read_text(encoding="utf-8")
    blocks = re.findall(
        r"## arena bare face — (\d{4}-\d{2}-\d{2}) — kevy ([\d.]+).*?\n\n(\| verb.*?)\n\nGap rule",
        text,
        re.S,
    )
    if not blocks:
        sys.exit("sync_readme_bench: no arena table in bench/PERF-LEDGER.md")
    # By date, not by position. It was `blocks[-1]`, which means "last in
    # the file" — true only while entries are appended in order, and the
    # ledger reads newest-first. Adding a newer entry at the top therefore
    # left this reading the OLD one and reporting success with the version
    # it had just not used.
    date, version, table = max(blocks, key=lambda b: b[0])
    rows = {}
    for line in table.split("\n")[2:]:
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 5:
            continue
        verb = cells[0]
        rows[verb] = {
            "kevy": int(cells[1].replace(",", "")),
            "redis8": int(cells[2].replace(",", "")),
            "valkey": int(cells[3].replace(",", "")),
            "dragonfly": int(cells[4].replace(",", "")),
        }
    if not rows:
        sys.exit("sync_readme_bench: the arena table parsed to nothing")
    return date, version, rows


def m(n):
    return f"{n / 1e6:.2f} M/s"


def build(date, version, rows):
    """The two tables, and the sentence that dates them."""
    get, setv = rows["GET"], rows["SET"]
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
                f"| {a} | kevy | valkey 9.1 | {b} |\n"
                f"|---|---:|---:|---|\n"
                f"| `GET -c 50 -P 16` | {m(get['kevy'])} | {m(get['valkey'])} | "
                f"**{get['kevy'] / get['valkey']:.2f}×** |\n"
                f"| `SET -c 50 -P 16` | {m(setv['kevy'])} | {m(setv['valkey'])} | "
                f"**{setv['kevy'] / setv['valkey']:.2f}×** |"
            ),
            "lead": (
                f"| {c} | {d} |\n"
                f"|---|---:|\n"
                f"| valkey 9.1 | **{get['kevy'] / get['valkey']:.2f}×** |\n"
                f"| redis 8 | **{get['kevy'] / get['redis8']:.2f}×** |\n"
                f"| dragonfly | **{get['kevy'] / get['dragonfly']:.2f}×** |"
            ),
            "rate": m(get["kevy"]),
        }
    return out



# ── the site quotes the same run, in five more places ────────────────────
#
# The three READMEs were tied to the ledger; the site was not. Its
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
    order = ["GET", "SET", "INCR", "SADD", "HSET", "LPUSH", "ZADD"]
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

        # The kevy-vs-valkey rows. Matched on their own two rows so the
        # replacement cannot land on the four-engine table below.
        s = re.sub(
            r"\| `GET -c 50 -P 16` \|[^\n]*\n\| `SET -c 50 -P 16` \|[^\n]*",
            "\n".join(t["vs"].split("\n")[2:]),
            s,
        )
        # The four-engine lead table.
        s = re.sub(
            r"\| valkey 9\.1 \| \*\*[\d.]+×\*\* \|\n\| redis 8 \| \*\*[\d.]+×\*\* \|\n"
            r"\| dragonfly \| \*\*[\d.]+×\*\* \|",
            "\n".join(t["lead"].split("\n")[2:]),
            s,
        )
        # The rate quoted in the prose beside it.
        s = re.sub(r"kevy at\n?\s*[\d.]+ M/s against each", f"kevy at {t['rate']} against each", s)
        s = re.sub(r"[\d.]+ M/s で各エンジンに対し", f"{t['rate']} で各エンジンに対し", s)
        s = re.sub(r"kevy 以 [\d.]+ M/s", f"kevy 以 {t['rate']}", s)

        if s != before:
            if check:
                stale.append(name)
            else:
                p.write_text(s, encoding="utf-8")

    stale += write_site(rows, version, check)

    if check:
        if stale:
            print(f"sync_readme_bench: STALE — {', '.join(stale)}")
            print(f"  The ledger's latest arena run is {date} (kevy {version}).")
            print("  Regenerate with: python3 tools/sync_readme_bench.py")
            sys.exit(1)
        print(f"ok: 3 READMEs and the site carry the {date} arena numbers (kevy {version})")
        return

    print(f"wrote the {date} arena numbers (kevy {version}) into 3 READMEs and the site")


if __name__ == "__main__":
    main()
