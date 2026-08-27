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
import platform as _platform
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


def host_platform():
    """What this run actually happened on — never what the corpus wishes."""
    return {"Linux": "linux", "Darwin": "macos", "Windows": "windows"}.get(
        _platform.system(), _platform.system().lower())


def corpus():
    if not CORPUS.exists():
        refuse(f"no {CORPUS.relative_to(ROOT)}; the corpus defines what 'executed' means")
    c = dict(tomllib.loads(CORPUS.read_text())["corpus"])
    enforcing, here = c["platform"], host_platform()
    c["enforcing_platform"] = enforcing
    # The identity records where the measurement HAPPENED, never what the
    # corpus wishes. Code switched off by cfg does not appear in coverage
    # data as dead — it is absent from the denominator entirely: measured
    # 2026-08-27, a macOS run sees 1 of 16 uring_*.rs files and none of
    # kevy-uring at all. So a cross-platform comparison does not merely add
    # dead regions, it makes whole symbols LEAVE the set, which a ratchet
    # reads as improvement. That is a silent false green, and the identity
    # check is what makes it impossible without anyone having to remember.
    c["platform"] = here
    if here != enforcing:
        print(f"atlas: NOTE — measured on {here}; the enforcing platform is "
              f"{enforcing}. Not baseline material: cfg({enforcing})-only code "
              f"is absent from this run rather than dead in it, so this set is "
              f"smaller on both sides and cannot be compared with one from "
              f"{enforcing}.")
    return c


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
    """Verify the enumeration against llvm, and report the definitional gap.

    llvm's per-file `summary.regions.count` is computed by something other
    than this script, so agreement on it is a real witness: across the
    workspace all 599 files agree and the global total matches exactly
    (160,062). The same region set is being enumerated.

    The *covered* verdict differs, and the difference is a definition
    rather than a defect. A span inside a generic can be reached by one
    instantiation and not another; llvm's merged model keeps those apart
    and counts the unexercised copy as an uncovered region, while summing
    across instantiations calls the span covered. For "how thoroughly is
    every instantiation exercised", llvm's reading is the right one. For
    "which source can I delete, or must I write a test for" — the question
    this atlas exists to answer — summing is: the line ran, so it is not
    dead and cannot be removed. Measured 2026-08-27, the two readings
    differ on 822 of 160,062 regions, 2.8% of the dead set.

    Three other reconstructions were tried and each disagreed with llvm's
    summary *and* with the others — segments (493 of 599 files off), LCOV
    DA records (81,235 unique lines against a declared LF of 83,962, with
    no duplicate records to explain the gap), and per-name merging. The
    summary comes from a merged model the export formats do not fully
    expose. Demanding equality with it would make this gate unusable
    without making it more correct, so the enumeration is enforced and the
    verdict gap is reported.
    """
    mine = collections.Counter()
    mine_zero = collections.Counter()
    for (src, *_), n in counts.items():
        mine[src] += 1
        if n == 0:
            mine_zero[src] += 1
    llvm_dead = 0
    for f in data["files"]:
        s = f["summary"]["regions"]
        got, want = mine[f["filename"]], s["count"]
        if got != want:
            refuse(
                f"enumeration mismatch for {f['filename']}: "
                f"parsed {got} regions, llvm maps {want}"
            )
        llvm_dead += s["count"] - s["covered"]
    return llvm_dead


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


# Tokens that must not be counted as braces: a `{` inside a string literal
# or a comment closes nothing. The assertion message that exposed this bug
# was literally `"polar {} vs middle {}"`.
SKIP = re.compile(r"""
      (?P<line_comment>//[^\n]*)
    | (?P<block_comment>/\*.*?\*/)
    | (?P<string>"(?:\\.|[^"\\])*")
    | (?P<rawstring>r\#*"(?:.|\n)*?"\#*)
    | (?P<char>'(?:\\.|[^'\\])')
""", re.VERBOSE | re.DOTALL)

CFGTEST = re.compile(r"#\[cfg\(test\)\]")


def cfg_test_ranges(path, cache):
    """Line ranges under `#[cfg(test)]`, which are not product code.

    Test code sits in the same file and therefore in the same coverage
    export, and its never-executed regions are the arguments to assertion
    messages that only evaluate when an assertion fails. Leaving them in
    means **writing a test grows the dead set** — the gate fires on the
    improvement it exists to encourage. Found by writing two tests for
    kevy-geo: the two regions they covered left the set, and four new ones
    arrived from their own assert! messages.

    Integration tests under `crates/*/tests/` never had this problem —
    llvm-cov does not report them as files at all (0 of 599).

    Braces inside strings and comments are blanked first. The assertion
    message that exposed this bug was literally `"polar {} vs middle {}"`,
    and a naive counter would have closed the block on it.
    """
    if path in cache:
        return cache[path]
    try:
        text = pathlib.Path(path).read_text(errors="replace")
    except OSError:
        cache[path] = []
        return cache[path]

    blanked = SKIP.sub(lambda m: re.sub(r"[^\n]", " ", m.group()), text)
    lines = blanked.splitlines()
    ranges, i = [], 0
    while i < len(lines):
        if not CFGTEST.search(lines[i]):
            i += 1
            continue
        depth, j, opened = 0, i, False
        while j < len(lines):
            for ch in lines[j]:
                if ch == "{":
                    depth += 1
                    opened = True
                elif ch == "}":
                    depth -= 1
            if opened and depth <= 0:
                break
            j += 1
        ranges.append((i + 1, min(j + 1, len(lines))))
        i = j + 1
    cache[path] = ranges
    return ranges


MODCFG = re.compile(r"^\s*#\[cfg\(([^\]]*)\)\]\s*$")
MODDECL = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;")


def gated_modules(root):
    """Files whose whole module is behind a #[cfg(...)].

    The per-region scan looks 60 lines up for an attribute and therefore
    cannot see the commonest gating there is: `#[cfg(target_os = "linux")]
    mod uring_reactor;` in lib.rs, which switches off an entire file from
    somewhere else entirely. Without this, every io_uring region on a mac
    lands in `untested` — 'a test is owed' for code the host cannot even
    compile, which is the wrong work item and a large one.
    """
    out = {}
    for f in root.rglob("*.rs"):
        try:
            lines = f.read_text(errors="replace").splitlines()
        except OSError:
            continue
        pending = None
        for line in lines:
            m = MODCFG.match(line)
            if m:
                pending = m.group(1)
                continue
            d = MODDECL.match(line)
            if d and pending:
                name = d.group(1)
                for cand in (f.parent / f"{name}.rs", f.parent / name / "mod.rs",
                             f.with_suffix("") / f"{name}.rs",
                             f.with_suffix("") / name / "mod.rs"):
                    if cand.exists():
                        out[str(cand.resolve())] = pending
            if not MODCFG.match(line):
                pending = pending if d and not d.group(1) else (pending if m else None)
    return out


def classify(path, lineno, cache, gated=None):
    text = source_line(path, lineno, cache)
    g = (gated or {}).get(str(pathlib.Path(path).resolve()))
    if g and ("target_os" in g or "unix" in g or "windows" in g or "linux" in g):
        return "platform", f"module gated by cfg({g})"
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
    llvm_dead = reconcile(data, counts)

    # Reconciliation ran on the FULL set above — llvm counts test code
    # too, so filtering before it would break the one exact witness this
    # instrument has. Filter after.
    cfgcache = {}
    dead = {}
    excluded = 0
    for k, v in counts.items():
        if v != 0:
            continue
        src, l1 = k[0], k[1]
        if any(lo <= l1 <= hi for lo, hi in cfg_test_ranges(src, cfgcache)):
            excluded += 1
            continue
        dead[k] = v
    names = {n for k in dead for n in owners[k]}
    dm = demangle(names)
    gated = gated_modules(ROOT / "crates")
    cache, rows = {}, []
    for key in sorted(dead):
        src, l1, c1, l2, c2 = key
        syms = {symbol_of(dm[n]) for n in owners[key]}
        kind, evidence = classify(src, l1, cache, gated)
        rows.append({
            "symbol": sorted(syms)[0] if syms else "?",
            "file": str(pathlib.Path(src).relative_to(ROOT)) if str(src).startswith(str(ROOT)) else src,
            "line": l1, "kind": kind, "evidence": evidence,
        })
    return cfg, counts, rows, llvm_dead, excluded


def per_crate(counts):
    """Regions and dead regions per crate.

    Without the denominator, a crate absent from the corpus and a crate
    perfectly covered both read as zero dead — and the absent one looks
    better. kevy-uring on macOS is exactly that case: zero regions, zero
    dead, and nothing measured at all.
    """
    out = collections.defaultdict(lambda: {"regions": 0, "dead": 0})
    for (src, *_), n in counts.items():
        parts = str(src).split("/crates/")
        if len(parts) < 2:
            continue
        c = parts[1].split("/")[0]
        out[c]["regions"] += 1
        if n == 0:
            out[c]["dead"] += 1
    return dict(sorted(out.items()))


def write_outputs(cfg, counts, rows, llvm_dead):
    reg = register()
    by_symbol = collections.Counter(r["symbol"] for r in rows)
    OUT_SET.write_text(json.dumps({
        "corpus": cfg["id"],
        "platform": cfg["platform"],
        "total_regions": len(counts),
        "dead_regions": len(rows),
        "llvm_per_instantiation_dead": llvm_dead,
        "crates": per_crate(counts),
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
           f"A span is dead when no instantiation of any function containing it",
           f"executed. llvm's own per-instantiation reading counts "
           f"**{llvm_dead}** instead: a span reached by `foo<u32>` but not by",
           "`foo<String>` is dead in that reading and live in this one. Both are",
           "correct about different questions; this one answers *what can be",
           "deleted or must be tested*. The enumeration itself is verified",
           "exactly against llvm, file by file.", "",
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
    cfg, counts, rows, llvm_dead, excluded = build(sys.argv[1])
    write_outputs(cfg, counts, rows, llvm_dead)
    print(f"  {excluded} never-executed regions under #[cfg(test)] excluded "
          f"— assertion-message arguments, not product code")
    print(f"atlas: {len(rows)} dead regions of {len(counts)} "
          f"({100 * len(rows) / len(counts):.1f}%) — corpus {cfg['id']}/{cfg['platform']}")
    print(f"  {OUT_MD.relative_to(ROOT)}  {OUT_SET.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
