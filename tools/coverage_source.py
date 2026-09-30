#!/usr/bin/env python3
"""What the dead-region atlas reads from the source itself: the line a
region starts on, the cfg attributes above it, the ranges under
`#[cfg(test)]`, and whole modules switched off from their parent."""

import pathlib
import re

CFG = re.compile(r"#\[cfg\(([^\]]*)\)\]")


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
