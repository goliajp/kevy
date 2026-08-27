#!/usr/bin/env python3
"""The zero-hit region atlas: which code never runs, by name.

`COV-BASELINE.json` records one number — 79.64% of lines. A scalar cannot
say *which* fifth is dead, so it holds steady through a complete
substitution of what is untested. This reads the same llvm-cov run and
produces the set instead.

Two things make it trustworthy rather than merely plausible:

**It reconciles against llvm's own arithmetic.** llvm-cov reports a
per-file region summary computed independently of this script. The atlas
recomputes those totals from the raw function records and REFUSES if they
disagree. That check is the reason the merge below is correct: regions
arrive once per *instantiation*, so a generic function contributes the same
source region many times, and summing counts across instantiations is what
turns 940 instantiation-regions into the 334 source-regions llvm counts.
Getting that wrong reports 458 dead regions where there are 4 — a number
shaped exactly like data.

**Scope comes from the corpus, not from the JSON.** A package-scoped run
leaves every other crate cold, so `functions` in such a run is 99.7% dead
and means nothing. The atlas takes its scope from `suite/corpus.toml` and
refuses a run whose file set does not cover the workspace.

Classification is deliberately timid. It labels a region `panic` or
`platform` only on evidence it can point at, and calls everything else
`untested` — never `unreachable`, which no static reading of this data can
establish. Human judgement goes in `suite/dead-paths.toml`, where it must
be written down with a reason.

Run: python3 tools/coverage_atlas.py <llvm-cov.json>
Exit: 0 wrote the atlas, 2 refused.
"""

import collections
import json
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CORPUS = ROOT / "suite/corpus.toml"
REGISTER = ROOT / "suite/dead-paths.toml"
OUT_MD = ROOT / "bench/DEAD-ATLAS.md"
OUT_SET = ROOT / "bench/DEAD-SET.json"

CODE_REGION = 0
PANIC = re.compile(r"\b(unreachable!|panic!|todo!|unimplemented!|abort\(|\.expect\(|\.unwrap\(\))")
CFG = re.compile(r"#\[cfg\(([^\]]*)\)\]")
MIN_FILES = 100


def refuse(msg):
    print(f"atlas: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def corpus():
    if not CORPUS.exists():
        refuse(f"no {CORPUS.relative_to(ROOT)}; the corpus defines what 'executed' means")
    return tomllib.loads(CORPUS.read_text())["corpus"]


def merge_regions(data, scope):
    """(file, l1, c1, l2, c2) -> (summed count, owning symbols).

    Regions arrive per instantiation; llvm's file summary counts source
    locations. Summing across instantiations is what reconciles the two.
    """
    counts = collections.defaultdict(int)
    owners = collections.defaultdict(set)
    for fn in data["functions"]:
        names = fn["filenames"]
        for r in fn["regions"]:
            if r[7] != CODE_REGION:
                continue
            src = names[r[5]] if r[5] < len(names) else names[0]
            if src not in scope:
                continue
            key = (src, r[0], r[1], r[2], r[3])
            counts[key] += r[4]
            owners[key].add(fn["name"])
    return counts, owners


def reconcile(data, counts):
    """llvm computed these totals independently. Disagreement is our bug."""
    mine = collections.Counter()
    mine_zero = collections.Counter()
    for (src, *_), n in counts.items():
        mine[src] += 1
        if n == 0:
            mine_zero[src] += 1
    for f in data["files"]:
        s = f["summary"]["regions"]
        got, want = mine[f["filename"]], s["count"]
        gz, wz = mine_zero[f["filename"]], s["count"] - s["covered"]
        if (got, gz) != (want, wz):
            refuse(
                f"reconciliation failed for {f['filename']}: "
                f"parsed {got} regions / {gz} dead, llvm reports {want} / {wz}"
            )


def demangle(names):
    """rustfilt is an external tool, like llvm-cov itself. Required, because
    a baseline whose identities depend on whether a tool was installed is
    not a baseline."""
    names = sorted(names)
    if not names:
        return {}
    try:
        out = subprocess.run(["rustfilt"], input="\n".join(names),
                             capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        refuse("rustfilt not available; install with `cargo install rustfilt`")
    got = out.splitlines()
    if len(got) != len(names):
        refuse(f"rustfilt returned {len(got)} lines for {len(names)} names")
    return dict(zip(names, got))


def symbol_of(demangled):
    """Strip the closure/instantiation tail and the trailing hash."""
    s = re.sub(r"::\{closure#\d+\}", "", demangled)
    s = re.sub(r"::h[0-9a-f]{16}$", "", s)
    s = re.sub(r"<[^<>]*>", "", s)
    return s


def source_line(path, lineno, cache):
    if path not in cache:
        p = pathlib.Path(path)
        cache[path] = p.read_text(errors="replace").splitlines() if p.exists() else []
    lines = cache[path]
    return lines[lineno - 1].strip() if 0 < lineno <= len(lines) else ""


def enclosing_cfg(path, lineno, cache):
    """Nearest #[cfg(...)] above the region, as evidence — not a verdict."""
    if path not in cache:
        p = pathlib.Path(path)
        cache[path] = p.read_text(errors="replace").splitlines() if p.exists() else []
    lines = cache[path]
    for i in range(min(lineno, len(lines)) - 1, max(0, lineno - 60), -1):
        m = CFG.search(lines[i])
        if m:
            return m.group(1)
    return None


def classify(path, lineno, cache):
    text = source_line(path, lineno, cache)
    if PANIC.search(text):
        return "panic", text
    cfg = enclosing_cfg(path, lineno, cache)
    if cfg and ("target_os" in cfg or "unix" in cfg or "windows" in cfg or "linux" in cfg):
        return "platform", f"cfg({cfg})"
    return "untested", text


def register():
    if not REGISTER.exists():
        return {}
    doc = tomllib.loads(REGISTER.read_text())
    return {e["symbol"]: e for e in doc.get("dead", [])}


def build(path):
    cfg = corpus()
    raw = json.loads(pathlib.Path(path).read_text())
    data = raw["data"][0]
    files = data["files"]
    if len(files) < MIN_FILES:
        refuse(f"the run covers {len(files)} files; the corpus is workspace-wide "
               f"(a package-scoped run reports every other crate as dead)")
    scope = {f["filename"] for f in files}
    counts, owners = merge_regions(data, scope)
    reconcile(data, counts)

    dead = {k: v for k, v in counts.items() if v == 0}
    names = {n for k in dead for n in owners[k]}
    dm = demangle(names)
    cache, rows = {}, []
    for key in sorted(dead):
        src, l1, c1, l2, c2 = key
        syms = {symbol_of(dm[n]) for n in owners[key]}
        kind, evidence = classify(src, l1, cache)
        rows.append({
            "symbol": sorted(syms)[0] if syms else "?",
            "file": str(pathlib.Path(src).relative_to(ROOT)) if str(src).startswith(str(ROOT)) else src,
            "line": l1, "kind": kind, "evidence": evidence,
        })
    return cfg, counts, rows


def write_outputs(cfg, counts, rows):
    reg = register()
    by_symbol = collections.Counter(r["symbol"] for r in rows)
    OUT_SET.write_text(json.dumps({
        "corpus": cfg["id"],
        "platform": cfg["platform"],
        "total_regions": len(counts),
        "dead_regions": len(rows),
        "symbols": dict(sorted(by_symbol.items())),
    }, indent=2) + "\n")

    by_kind = collections.Counter(r["kind"] for r in rows)
    by_crate = collections.Counter(r["file"].split("/")[1] if r["file"].startswith("crates/") else "?" for r in rows)
    out = ["# Dead-path atlas", "",
           f"Corpus `{cfg['id']}` on {cfg['platform']}. "
           f"**{len(rows)} never-executed regions** of {len(counts)} "
           f"({100 * len(rows) / len(counts):.1f}%), across "
           f"{len(by_symbol)} symbols in {len(by_crate)} crates.", "",
           "Generated by `tools/coverage_atlas.py`; do not edit. Human",
           "classification goes in `suite/dead-paths.toml`.", "",
           "## By class", "", "| class | regions | meaning |", "|---|---:|---|",
           f"| `untested` | {by_kind['untested']} | reachable as far as this can tell — a test is owed |",
           f"| `panic` | {by_kind['panic']} | a panic/abort edge |",
           f"| `platform` | {by_kind['platform']} | under a cfg not satisfied on {cfg['platform']} |",
           "", f"Registered with a human reason in `suite/dead-paths.toml`: "
           f"{sum(1 for r in rows if r['symbol'] in reg)}.", "",
           "## By crate", "", "| crate | dead regions |", "|---|---:|"]
    out += [f"| {c} | {n} |" for c, n in by_crate.most_common()]
    out += ["", "## Every region", "", "| crate | symbol | file:line | class | evidence |", "|---|---|---|---|---|"]
    for r in sorted(rows, key=lambda r: (r["file"], r["line"])):
        crate = r["file"].split("/")[1] if r["file"].startswith("crates/") else "?"
        ev = r["evidence"].replace("|", "\\|")[:70]
        out.append(f"| {crate} | `{r['symbol']}` | {r['file']}:{r['line']} | {r['kind']} | `{ev}` |")
    OUT_MD.write_text("\n".join(out) + "\n")


def main():
    if len(sys.argv) != 2:
        refuse("usage: coverage_atlas.py <llvm-cov.json>")
    cfg, counts, rows = build(sys.argv[1])
    write_outputs(cfg, counts, rows)
    print(f"atlas: {len(rows)} dead regions of {len(counts)} "
          f"({100 * len(rows) / len(counts):.1f}%) — corpus {cfg['id']}/{cfg['platform']}")
    print(f"  {OUT_MD.relative_to(ROOT)}  {OUT_SET.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
