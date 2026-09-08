#!/usr/bin/env python3
"""Where this repository stands against the rulebook, rule by rule.

The rulebook (`~/.claude-shared/global/methodology/module-craft.md`) has
50 rules. Six of them are locked by a gate here. The rest had never been
counted, which meant "we follow the rulebook" was a claim with no
reading behind it — and a claim like that is the thing the rulebook
exists to replace.

This produces the reading. Three verdicts, and the third is not a
failure:

  LOCKED    a gate refuses a violation; the count is 0 by construction
  COUNTED   measurable here, with the number
  READING   not mechanically decidable — what a reader looks for is
            named, so the rule is still actionable

A rule in READING is not an excuse. `naming/no-lying-name` cannot be
regexed and is still the most expensive rule in the book. What this
file refuses to do is pretend a heuristic is a verdict: a counter that
is wrong in either direction is worse than an honest "read this".
"""

import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = sorted(ROOT.glob("crates/*/src/**/*.rs"))


def is_test(p):
    s = str(p)
    return (
        "/tests/" in s
        or p.name == "tests.rs"
        or p.name.endswith("_tests.rs")
        or "/benches/" in s
        or "/fuzz/" in s
    )


PROD = [p for p in SRC if not is_test(p)]


def read(p):
    return p.read_text(encoding="utf-8", errors="replace")


def fn_signatures(text):
    """(name, params-text) for every `fn` — brace-depth aware on the args."""
    for m in re.finditer(r"\bfn\s+(\w+)\s*(?:<[^>]*>)?\s*\(", text):
        i = m.end()
        depth, start = 1, i
        while i < len(text) and depth:
            c = text[i]
            depth += (c in "([<") - (c in ")]>")
            i += 1
        yield m.group(1), text[start : i - 1]


def count_params(args):
    if not args.strip():
        return 0
    depth, n = 0, 1
    for c in args:
        depth += c in "<(["
        depth -= c in ">)]"
        if c == "," and depth == 0:
            n += 1
    return n - (1 if re.match(r"\s*&?\s*(mut\s+)?self\b", args) else 0)


# ── the measurements ────────────────────────────────────────────────

def m_params_over_5():
    hits = [
        (p, name)
        for p in PROD
        for name, args in fn_signatures(read(p))
        if count_params(args) > 5
    ]
    return len(hits), "functions taking more than 5 parameters"


def m_bare_bool():
    hits = [
        (p, name)
        for p in PROD
        for name, args in fn_signatures(read(p))
        if args.count(",") >= 1 and re.search(r":\s*bool\b", args)
    ]
    return len(hits), "bool parameters that are not the only parameter"


def m_unit_in_name():
    # A quantity whose name carries no unit. The suffix list is the one
    # the codebase already uses, so this counts drift from its own
    # convention rather than from an imagined one.
    QTY = re.compile(r"\b(\w*?(timeout|delay|interval|size|limit|budget|elapsed))\s*:")
    UNIT = re.compile(r"_(ms|us|micros|nanos|ns|s|secs|bytes|b|kb|mb|pct|ratio)$|Duration|Instant|ByteSize")
    n = 0
    for p in PROD:
        for m in QTY.finditer(read(p)):
            if not UNIT.search(m.group(1)):
                n += 1
    return n, "quantities whose name carries no unit (Duration/Instant excluded)"


def m_catchall_match():
    n = sum(len(re.findall(r"^\s*_\s*=>", read(p), re.M)) for p in PROD)
    return n, ("`_ =>` arms — SITES TO JUDGE: required on a foreign "
            "#[non_exhaustive] enum, a hole on one of ours")


def m_stringly():
    n = sum(len(re.findall(r'[=!]=\s*"[a-z_]{3,}"', read(p))) for p in PROD)
    return n, "branches taken on a string literal comparison"


def m_mod_rs_logic():
    """An assembly point is a file that assembles — one declaring
    submodules. A `lib.rs` with none IS the crate's single file, and the
    rule's reason ("nobody knows where to find that logic") does not
    apply to it: there is one place, and this is it. Counting those was
    the first version of this measurement, and it read 38 files where
    the rule means 32.
    """
    bad, fns = [], 0
    for p in PROD:
        if p.name not in ("mod.rs", "lib.rs"):
            continue
        body = re.sub(r"//.*", "", read(p))
        submods = re.findall(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*;", body, re.M)
        bodies = re.findall(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+\w+[^;{]*\{",
            body, re.M)
        if submods and bodies:
            bad.append(p)
            fns += len(bodies)
    return len(bad), f"assembly points carrying logic, {fns} functions between them"


def m_blocking_in_async():
    n = 0
    for p in PROD:
        t = read(p)
        for m in re.finditer(r"\basync\s+fn\s+\w+", t):
            tail = t[m.end() : m.end() + 3000]
            n += len(re.findall(r"std::thread::sleep|std::fs::(read|write|File)", tail))
    return n, "blocking calls inside the first 3 KB of an async fn"


def m_out_params():
    hits = [
        (p, name)
        for p in PROD
        for name, args in fn_signatures(read(p))
        if re.search(r":\s*&mut\s+(Vec|String|HashMap)", args)
    ]
    return len(hits), ("&mut collection parameters — SITES TO JUDGE, not violations: "
            "the rule allows a reused buffer on a hot path when the doc says why")


def m_pub_surface():
    per = {}
    for p in PROD:
        crate = p.relative_to(ROOT / "crates").parts[0]
        per[crate] = per.get(crate, 0) + len(re.findall(r"^\s*pub\s+(fn|struct|enum|trait|const|type)\b", read(p), re.M))
    over = {c: n for c, n in per.items() if n > 40}
    return len(over), f"crates with more than 40 public items (largest: {max(per, key=per.get)} {max(per.values())})"


def gate_clean(args):
    r = subprocess.run(["cargo", "clippy", "--workspace", *args], cwd=ROOT,
                       capture_output=True, text=True)
    return r.returncode == 0


RULES = [
    # (id, scope, verdict, measurement-or-note)
    ("naming/no-lying-name", "全", "READING", "a name promising what the body does not do; no signal a regex can see"),
    ("naming/verb-does-that-verb", "全", "READING", "get_* with a side effect, is_* returning non-bool"),
    ("naming/bool-reads-as-assertion", "全", "READING", "flag / status / check as a bool's whole name"),
    ("naming/unit-in-the-name", "全", m_unit_in_name, None),
    ("naming/no-novel-abbrev", "全", "READING", "abbreviations outside the domain's own set"),
    ("naming/same-thing-same-name", "全", "READING", "one concept under conn / session / client / peer"),
    ("fn/max-50-lines", "全", "LOCKED", "locgate"),
    ("fn/one-thing", "全", "READING", "a name with `and`; two blocks split by a comment"),
    ("fn/max-5-params", "全", m_params_over_5, None),
    ("fn/no-bare-bool-param", "全", m_bare_bool, None),
    ("fn/result-not-panic", "石钢边", "LOCKED", "panic-free"),
    ("fn/no-out-param", "全", m_out_params, None),
    ("mod/max-500-lines", "全", "LOCKED", "locgate"),
    ("mod/one-responsibility", "全", "READING", "a file whose one-line description needs an `and`"),
    ("mod/minimal-pub-surface", "石钢", m_pub_surface, None),
    ("mod/mod-rs-assembles-only", "全", m_mod_rs_logic, None),
    ("mod/deps-point-down", "全", "LOCKED", "architecture (suite/architecture.toml)"),
    ("mod/no-second-implementation", "全", "READING", "two implementations of one capability"),
    ("type/illegal-states-unrepresentable", "石钢边", "READING", "a bool+Option pair whose four combinations include two illegal ones"),
    ("type/newtype-over-primitive", "石钢", "READING", "two same-typed scalars in one signature"),
    ("type/no-catchall-match", "石钢", m_catchall_match, None),
    ("type/no-stringly-typed", "全", m_stringly, None),
    ("err/errors-are-values", "石钢水", "LOCKED", "panic-free"),
    ("err/says-what-and-which", "全", "READING", "an error carrying no value"),
    ("err/no-swallow", "全", "LOCKED", "panic-free (let_underscore_must_use)"),
    ("err/convert-at-the-boundary", "石钢", "READING", "map_err repeated down a call chain"),
    ("inv/write-it-down", "石钢边", "READING", "an invariant the code relies on and does not state"),
    ("inv/prefer-type-over-comment", "石钢", "READING", "a comment asserting what a type could"),
    ("doc/why-not-what", "全", "READING", "a comment restating the line under it"),
    ("doc/comment-drift-is-a-bug", "全", "READING", "a comment the code stopped matching"),
    ("doc/pub-doc-is-a-contract", "石钢", "LOCKED", "panic-free (missing_docs); 100% documented"),
    ("doc/example-is-a-test", "石", "LOCKED", "doctestgate — the ratchet, not zero: 5.4% carry one"),
    ("test/separate-pure-from-io", "全", "READING", "logic reachable only through a socket or a file"),
    ("test/inject-clock-rand-fs", "石钢", "READING", "SystemTime::now / rand / std::fs inside library code"),
    ("test/instrument-must-be-able-to-fail", "装", "READING", "a check that passes on empty input"),
    ("test/one-question-per-test", "全", "READING", "a test name that does not say what it asks"),
    ("test/no-assert-on-what-you-set", "装", "READING", "a test asserting the value it just wrote"),
    ("unsafe/every-block-has-a-safety-note", "边石", "LOCKED", "undocumented_unsafe_blocks = deny"),
    ("unsafe/safety-states-the-premise", "边石", "READING", "a SAFETY note that says `this is safe`"),
    ("unsafe/minimize-the-area", "边石", "READING", "an unsafe block wider than the operation needing it"),
    ("unsafe/forbid-op-in-unsafe-fn", "边石", "COUNTED-BELOW", None),
    ("boundary/validate-foreign-input", "边", "READING", "an FFI entry that trusts a pointer or a length"),
    ("boundary/no-panic-across-abi", "边", "READING", "an extern fn without catch_unwind"),
    ("boundary/own-the-free", "边", "READING", "an ownership handoff whose doc names no releasing function"),
    ("conc/state-has-an-owner", "石钢", "READING", "shared state whose writer is not named"),
    ("conc/lock-order-written-down", "石钢", "READING", "two locks held together with no stated order"),
    ("conc/no-blocking-in-async", "全", m_blocking_in_async, None),
    ("perf/no-optimize-before-measure", "全", "READING", "an optimisation with no measurement beside it"),
    ("perf/hot-path-alloc-count-is-known", "石", "READING", "a hot path whose allocation count is not written down"),
    ("pattern/name-what-it-eliminates", "全", "READING", "a pattern introduced without naming what it removed"),
]


def main() -> int:
    rows, counted, locked, reading = [], 0, 0, 0
    for rid, scope, verdict, extra in RULES:
        if callable(verdict):
            n, what = verdict()
            rows.append((rid, scope, "COUNTED", f"{n} — {what}"))
            counted += 1
        elif verdict == "LOCKED":
            rows.append((rid, scope, "LOCKED", extra))
            locked += 1
        elif verdict == "COUNTED-BELOW":
            ws = "unsafe_op_in_unsafe_fn" in (ROOT / "Cargo.toml").read_text()
            per = sum(1 for p in PROD if "unsafe_op_in_unsafe_fn" in read(p))
            note = ("declared once at the workspace" if ws
                    else f"NOT at the workspace; {per} crates declare it alone")
            if ws and per:
                note += f"; {per} crates still repeat it"
            rows.append((rid, scope, "LOCKED" if ws else "COUNTED", note))
            if rows[-1][2] == "LOCKED":
                locked += 1
            else:
                counted += 1
        else:
            rows.append((rid, scope, "READING", extra))
            reading += 1

    out = ROOT / "quality/RULEBOOK-STATUS.md"
    with out.open("w") as f:
        f.write("# The rulebook against this repository\n\n")
        f.write(f"{len(RULES)} rules. **{locked} locked by a gate**, "
                f"**{counted} counted here**, **{reading} needing a reader**.\n\n")
        f.write("Generated by `tools/rulebook_audit.py`; re-run it rather than "
                "editing the table.\n\n")
        f.write("| rule | scope | status | reading |\n|---|---|---|---|\n")
        for rid, scope, st, note in rows:
            f.write(f"| `{rid}` | {scope} | **{st}** | {note} |\n")

    print(f"rulebook: {len(RULES)} rules — {locked} LOCKED, {counted} COUNTED, {reading} READING")
    for rid, scope, st, note in rows:
        if st == "COUNTED":
            print(f"  {rid:42s} {note}")
    print(f"\n-> {out.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
