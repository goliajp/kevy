#!/usr/bin/env python3
"""Where the same code exists twice.

v6 forbids "a complex implementation where a simpler one delivers the same
capability". That is a statement about a *relation* between two places in
the code, and no baseline in this repository has ever stored a relation.

This is the instrument, and deliberately only an instrument. Whether this
codebase has meaningful duplication has not been measured, and the
methodology's Pre-Phase-B rule forbids building on plausible: an attack
whose target has not been shown to be a double-digit share is hand-waving.
So the atlas runs and is read, and only then is it worth asking whether a
gate belongs anywhere near this. "A register of deliberate twins and no
gate at all" stays a legitimate answer.

Method: winnowing (Schleimer, Wilkerson, Aiken 2003) — the standard, and
dependency-free. Rust source is lexed to a token stream; identifiers and
literals are normalised so a renamed copy still matches (a type-2 clone);
overlapping k-grams are hashed; and in each window of w hashes the minimum
is kept as a fingerprint. That guarantees any shared run of at least
k + w - 1 tokens produces at least one shared fingerprint, while storing a
fraction of the hashes.

What it cannot do is find two *different* implementations of one capability
— the case the v6 goal is really about. Those share no tokens. This finds
the copies; F2, the differential harness, is what can speak to the rest.

Run: python3 tools/clone_atlas.py [--min-shared N]
Exit: 0 wrote the atlas, 2 refused.
"""

import collections
import hashlib
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "bench/CLONE-ATLAS.md"

K = 30          # tokens per k-gram: shorter finds boilerplate, not designs
W = 20          # winnowing window
MIN_SHARED = 4  # fingerprints two regions must share to be reported
MIN_FILES = 100

TOKEN = re.compile(r"""
      (?P<ws>\s+)
    | (?P<line_comment>//[^\n]*)
    | (?P<block_comment>/\*.*?\*/)
    | (?P<string>"(?:\\.|[^"\\])*")
    | (?P<char>'(?:\\.|[^'\\])')
    | (?P<lifetime>'[A-Za-z_][A-Za-z0-9_]*)
    | (?P<number>\b\d[\dA-Za-z_.]*)
    | (?P<ident>\b[A-Za-z_][A-Za-z0-9_]*\b)
    | (?P<op>[^\sA-Za-z0-9_])
""", re.VERBOSE | re.DOTALL)

# Kept verbatim: these carry the shape of the code. Everything else that is
# an identifier becomes `I`, so a renamed copy still matches.
KEYWORDS = {
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop",
    "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "async", "await", "box", "unreachable", "panic",
}


def refuse(msg):
    print(f"clones: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def lex(text):
    """-> [(normalised_token, line)]; comments and layout dropped."""
    out, line = [], 1
    for m in TOKEN.finditer(text):
        kind = m.lastgroup
        piece = m.group()
        if kind in ("ws", "line_comment", "block_comment"):
            line += piece.count("\n")
            continue
        if kind == "ident":
            out.append((piece if piece in KEYWORDS else "I", line))
        elif kind in ("string", "char", "number", "lifetime"):
            out.append(("L", line))
        else:
            out.append((piece, line))
        line += piece.count("\n")
    return out


def fingerprints(tokens):
    """Winnowed (hash, line) pairs."""
    if len(tokens) < K + W:
        return []
    grams = []
    for i in range(len(tokens) - K + 1):
        # Not the builtin hash(): it is salted per process, so the same
        # tree fingerprints differently every run. Measured before this was
        # fixed: three runs of the same tree reported 987, 1,034 and 1,011
        # pairs. An instrument whose reading changes when nothing changed is
        # not an instrument.
        h = int.from_bytes(hashlib.blake2b(
            "\x00".join(t for t, _ in tokens[i:i + K]).encode(),
            digest_size=8).digest(), "big")
        grams.append((h, tokens[i][1]))
    out, prev = [], None
    for i in range(len(grams) - W + 1):
        window = grams[i:i + W]
        best = min(range(W - 1, -1, -1), key=lambda j: (window[j][0], -j))
        pick = (i + best, window[best])
        if pick[0] != prev:
            out.append(pick[1])
            prev = pick[0]
    return out


def sources():
    files = [p for p in (ROOT / "crates").rglob("*.rs")
             if "/target/" not in str(p) and "/vendor/" not in str(p)]
    if len(files) < MIN_FILES:
        refuse(f"found {len(files)} source files under crates/; "
               f"the selector is broken, not the tree empty")
    return files


def crate_of(p):
    parts = str(p.relative_to(ROOT)).split("/")
    return parts[1] if len(parts) > 2 and parts[0] == "crates" else "?"


def main():
    min_shared = MIN_SHARED
    if "--min-shared" in sys.argv:
        min_shared = int(sys.argv[sys.argv.index("--min-shared") + 1])

    index = collections.defaultdict(list)
    per_file = {}
    total_tokens = 0
    for p in sources():
        try:
            toks = lex(p.read_text(errors="replace"))
        except OSError:
            continue
        total_tokens += len(toks)
        fps = fingerprints(toks)
        per_file[p] = len(fps)
        for h, line in fps:
            index[h].append((p, line))

    if not index:
        refuse("no fingerprints produced; every file was shorter than a k-gram")

    # Fingerprints shared by very many places are boilerplate (a derive, a
    # match arm shape), not a design copied twice. Ignore them rather than
    # let them dominate every pair.
    pairs = collections.Counter()
    lines = collections.defaultdict(list)
    for h, hits in index.items():
        files = {p for p, _ in hits}
        if len(files) < 2 or len(files) > 8:
            continue
        ordered = sorted(files, key=str)
        for i, a in enumerate(ordered):
            for b in ordered[i + 1:]:
                pairs[(a, b)] += 1
                la = min(l for p, l in hits if p == a)
                lb = min(l for p, l in hits if p == b)
                lines[(a, b)].append((la, lb))

    hot = [(ab, n) for ab, n in pairs.items() if n >= min_shared]
    hot.sort(key=lambda t: (crate_of(t[0][0]) == crate_of(t[0][1]), -t[1]))

    cross = [t for t in hot if crate_of(t[0][0]) != crate_of(t[0][1])]
    out = [
        "# Clone atlas", "",
        f"Winnowed type-2 fingerprints over {len(per_file)} files "
        f"({total_tokens:,} tokens), k={K}, w={W}. A pair is listed when it "
        f"shares at least {min_shared} fingerprints — roughly "
        f"{K + W - 1} tokens of matching shape, with identifiers and "
        f"literals normalised so a renamed copy still matches.", "",
        "Generated by `tools/clone_atlas.py`; do not edit.", "",
        "**This is an instrument, not a gate.** It finds code that was",
        "copied. It cannot find two *different* implementations of one",
        "capability — those share no tokens — which is the case the v6 goal",
        "is really about; that is what the differential harness is for.",
        "Read this before deciding whether any dedup gate is warranted at all.",
        "",
        f"**{len(hot)} pairs** above the threshold, of which "
        f"**{len(cross)} cross a crate boundary** — two crates solving one",
        "problem twice is the shape worth looking at first.", "",
        "## Cross-crate pairs", "",
        "| shared | crate A | crate B | first match |", "|---:|---|---|---|",
    ]
    for (a, b), n in cross[:60]:
        la, lb = sorted(lines[(a, b)])[0]
        out.append(f"| {n} | `{a.relative_to(ROOT)}:{la}` | "
                   f"`{b.relative_to(ROOT)}:{lb}` | {crate_of(a)} ↔ {crate_of(b)} |")
    if not cross:
        out.append("| — | *none* | | |")
    same = [t for t in hot if crate_of(t[0][0]) == crate_of(t[0][1])]
    out += ["", f"## Within one crate ({len(same)} pairs)", "",
            "| shared | file A | file B |", "|---:|---|---|"]
    for (a, b), n in same[:40]:
        out.append(f"| {n} | `{a.relative_to(ROOT)}` | `{b.relative_to(ROOT)}` |")
    if not same:
        out.append("| — | *none* | |")
    OUT.write_text("\n".join(out) + "\n")
    print(f"clones: {len(hot)} pairs >= {min_shared} shared fingerprints "
          f"({len(cross)} cross-crate) over {len(per_file)} files -> "
          f"{OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
