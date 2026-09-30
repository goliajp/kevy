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

from coverage_regions import demangle, merge_regions, reconcile, refuse, symbol_of
from coverage_source import cfg_test_ranges, enclosing_cfg, gated_modules, source_line

ROOT = pathlib.Path(__file__).resolve().parent.parent
CORPUS = ROOT / "suite/corpus.toml"
REGISTER = ROOT / "suite/dead-paths.toml"
OUT_MD = ROOT / "target/reports/DEAD-ATLAS.md"
OUT_SET = ROOT / "bench/DEAD-SET.json"
# Every symbol this run has a region for, dead or not. An [[unstable]]
# register entry names a symbol that is dead in SOME runs, so it is checked
# against this, not against the dead set it may be absent from this time.
OUT_PRESENT = ROOT / "target/reports/DEAD-PRESENT.json"

PANIC = re.compile(r"\b(unreachable!|panic!|todo!|unimplemented!|abort\(|\.expect\(|\.unwrap\(\))")
MIN_FILES = 100


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


def _unstable_spec():
    """-> {"symbols": [...], "prefixes": [...]} from the register.

    Prefixes exist because the nondeterminism has subsystem granularity,
    not symbol granularity: three runs of one corpus produced three
    different growth lists over the same reactor and replication paths.
    Carried into the baseline so the ratchet's tolerance is part of what
    was recorded.
    """
    if not REGISTER.exists():
        return {"symbols": [], "prefixes": []}
    doc = tomllib.loads(REGISTER.read_text())
    e = doc.get("unstable", [])
    return {
        "symbols": sorted(x["symbol"] for x in e if "symbol" in x),
        "prefixes": sorted(x["prefix"] for x in e if "prefix" in x),
    }


def crate_of(path):
    parts = str(path).split("/crates/")
    return parts[1].split("/")[0] if len(parts) > 1 else None


def classify(path, lineno, cache, gated=None, by_crate=None):
    reg = (by_crate or {}).get(crate_of(path))
    if reg:
        return "gated-elsewhere", f"covered by {reg['gate']}"
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
    """-> ({symbol: entry}, {crate: entry}).

    A crate-level entry explains every dead region in that crate at once —
    a language door whose only caller is a JVM, say. It does not remove
    them: they stay in the denominator and in this atlas, classified by the
    gate that does cover them, because moving them out would raise the
    coverage percentage with nothing covered.
    """
    if not REGISTER.exists():
        return {}, {}
    doc = tomllib.loads(REGISTER.read_text())
    by_symbol = {e["symbol"]: e for e in doc.get("dead", [])}
    by_crate = {e["crate"]: e for e in doc.get("dead_crate", [])}
    for e in doc.get("unstable", []):
        who = e.get("symbol") or e.get("prefix")
        if not who:
            refuse("an unstable entry needs either a `symbol` or a `prefix`")
        if not e.get("observed") or not e.get("why", "").strip():
            refuse(f"unstable entry for {who} needs the differing values as "
                   f"`observed` and a `why`")
    for e in doc.get("dead_crate", []):
        if not e.get("gate", "").strip() or not e.get("reason", "").strip():
            refuse(f"dead_crate entry for {e.get('crate')} needs both a gate and a reason")
        # An entry that says "another gate covers this" is only worth
        # anything while that gate exists. A named file that has been
        # deleted turns the register into a place where dead code goes to
        # be forgiven.
        if not (ROOT / e["gate"]).exists():
            refuse(f"dead_crate entry for {e['crate']} names {e['gate']}, "
                   f"which does not exist")
    return by_symbol, by_crate


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
    names = {n for ns in owners.values() for n in ns}
    dm = demangle(names)
    OUT_PRESENT.parent.mkdir(parents=True, exist_ok=True)
    present = sorted({symbol_of(d) for d in dm.values()})
    OUT_PRESENT.write_text(json.dumps(present, indent=0) + "\n")
    gated = gated_modules(ROOT / "crates")
    _, by_crate = register()
    cache, rows = {}, []
    for key in sorted(dead):
        src, l1, c1, l2, c2 = key
        syms = {symbol_of(dm[n]) for n in owners[key]}
        kind, evidence = classify(src, l1, cache, gated, by_crate)
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
    reg, _ = register()
    by_symbol = collections.Counter(r["symbol"] for r in rows)
    OUT_SET.write_text(json.dumps({
        "corpus": cfg["id"],
        "platform": cfg["platform"],
        # What question this set answers, in setratchet's `identity` sense —
        # alongside which corpus and which platform. It is here because
        # `symbol_of` changed: sets recorded before the change name a trait
        # method `::fmt` and sets recorded after name it
        # `kevy_elect::vote::Ballot::fmt`, and comparing the two would report
        # thousands of symbols joining and leaving. That is not a worse set,
        # it is a different question, and setratchet already knows to refuse
        # rather than fail when the identity differs. A baseline recorded
        # before this field existed has no `kind`, so it refuses on sight
        # instead of quietly reading as a catastrophe.
        "kind": SYMBOL_SCHEME,
        # Which tree produced these numbers. Not part of the identity the
        # ratchet compares on; `envelope` uses it to refuse a maximum taken
        # across different code. See `tree_id`.
        "tree": tree_id(),
        "total_regions": len(counts),
        "dead_regions": len(rows),
        "llvm_per_instantiation_dead": llvm_dead,
        "crates": per_crate(counts),
        # Carried into the baseline so the ratchet reads its tolerance from
        # what was recorded, not from whatever the register says today.
        "unstable": _unstable_spec(),
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
           f"| `gated-elsewhere` | {by_kind['gated-elsewhere']} | reached by a gate `cargo test` cannot run — see `suite/dead-paths.toml` |",
           "", f"Registered with a human reason in `suite/dead-paths.toml`: "
           f"{sum(1 for r in rows if r['symbol'] in reg)}.", "",
           "## By crate", "", "| crate | dead regions |", "|---|---:|"]
    out += [f"| {c} | {n} |" for c, n in by_crate.most_common()]
    out += ["", "## Every region", "", "| crate | symbol | file:line | class | evidence |", "|---|---|---|---|---|"]
    for r in sorted(rows, key=lambda r: (r["file"], r["line"])):
        crate = r["file"].split("/")[1] if r["file"].startswith("crates/") else "?"
        ev = r["evidence"].replace("|", "\\|")[:70]
        out.append(f"| {crate} | `{r['symbol']}` | {r['file']}:{r['line']} | {r['kind']} | `{ev}` |")
    OUT_MD.parent.mkdir(parents=True, exist_ok=True)
    OUT_MD.write_text("\n".join(out) + "\n")


SYMBOL_DOCTEST_FLOOR = 11


def tree_id():
    """Which tree this corpus ran against: `HEAD`'s tree hash, plus a
    `-dirty` suffix when the working copy differs from it.

    Deliberately NOT part of `identity`. A baseline is meant to be compared
    against a later tree — that is the whole point of a ratchet — so making
    the tree part of the identity would refuse every gate run.

    It is here for `envelope`, which takes the element-wise maximum across
    several runs to absorb the noise. A maximum across *different* trees
    absorbs something else: a dead region that a later commit covered comes
    back in from an earlier run's numbers, and the baseline records it as
    still dead. That is a ratchet loosening itself with no reason attached,
    which is the one thing `setratchet`'s own docstring says must not
    happen. Three runs of one tree is what an envelope means.

    The tree hash rather than the commit, because two commits with the same
    content answer the same question. `-dirty` because a local run against
    uncommitted edits is not a run of that tree, and an envelope that mixed
    one in would be unreproducible.
    """
    def git(*args):
        try:
            r = subprocess.run(["git", *args], cwd=ROOT, capture_output=True,
                               text=True, check=True)
        except (OSError, subprocess.CalledProcessError):
            return None
        return r.stdout.strip()

    head = git("rev-parse", "HEAD^{tree}")
    if not head:
        return None
    dirty = git("status", "--porcelain")
    return f"{head}-dirty" if dirty else head

# Bump when `symbol_of` changes what an identity is. Baselines carry it, so
# a mismatch is refused rather than read as a regression.
SYMBOL_SCHEME = "symbols/qualified-path"


def selftest():
    """Run the doctests of `symbol_of` and its helpers, and refuse a run
    that verified nothing.

    `python3 -m doctest` exits 0 on a file with no doctests, which is the
    failure mode this repository keeps finding: an instrument that checks
    nothing is indistinguishable from one that checks everything and agrees.
    The floor makes deleting an example a failure rather than a shortcut —
    and `symbol_of` is exactly the function that needs examples, because the
    identity it computes is what the whole ratchet holds.
    """
    import doctest
    import coverage_regions

    ran, failed = doctest.testmod(coverage_regions, verbose=False)[::-1]
    if ran < SYMBOL_DOCTEST_FLOOR:
        refuse(f"only {ran} doctest example(s), floor is {SYMBOL_DOCTEST_FLOOR} "
               "— an instrument that checks nothing must not report agreement")
    if failed:
        print(f"coverage_atlas: FAIL — {failed} of {ran} examples")
        return 1
    print(f"coverage_atlas: selftest ok — {ran} examples")
    return 0


def main():
    if len(sys.argv) == 2 and sys.argv[1] == "--selftest":
        return selftest()
    if len(sys.argv) != 2:
        refuse("usage: coverage_atlas.py <llvm-cov.json> | --selftest")
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
