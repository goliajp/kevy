#!/usr/bin/env bash
# locgate — the LOC-debt ratchet (v3.18 T3).
#
# Hard rules (project CLAUDE.md): src files ≤ 500 LOC; functions
# ≤ 50 LOC unless the line right above `fn` carries a waiver comment
# naming the reason. Two sanctioned classes: (1) pure data-driven
# dispatch/match tables; (2) vendored third-party engine core —
# byte-identical code forked from a sibling project (splitting
# upstream's tested functions injects bugs without a readability
# gain). Test files and tests/ trees are exempt by convention. This
# gate turns the rules from prose into CI.
#
# Usage: bash bench/locgate.sh
set -u
cd "$(dirname "$0")/.."

python3 - <<'PY'
import re, glob, sys

FAIL = []

files = sorted(set(glob.glob('crates/*/src/**/*.rs', recursive=True)))
def exempt_file(f):
    return '/tests' in f or f.endswith('_tests.rs') or '/bin/' in f and False or 'tests_' in f.split('/')[-1]

# ---- rule 1: files ≤ 500 LOC
for f in files:
    if exempt_file(f):
        continue
    n = sum(1 for _ in open(f, errors='replace'))
    if n > 500:
        FAIL.append(f"file>{500}: {f} ({n})")

# ---- rule 2: fn ≤ 50 LOC or waivered
WAIVER = re.compile(r'(?:^|\s)(?:LOC-WAIVER|loc-waiver)\b.*:', re.I)
# Brace-literal chars (`'{'` / `b'}'`) would corrupt the depth count —
# blank them before counting.
# Neutralize braces that live inside line comments, string literals, and
# char literals so they don't skew the body brace-depth count (a function
# containing `{` / `}` in a string or comment is not thereby "longer").
LINE_COMMENT = re.compile(r'//.*')
STR_LIT = re.compile(r'"(?:\\.|[^"\\])*"')
CHAR_LIT = re.compile(r"'(?:\\.|[^'\\])*'")
def strip_braces_in_literals(line):
    line = LINE_COMMENT.sub('', line)
    line = STR_LIT.sub('""', line)
    line = CHAR_LIT.sub("''", line)
    return line
for f in files:
    if exempt_file(f):
        continue
    src = open(f, errors='replace').read()
    lines = src.split('\n')
    for m in re.finditer(r'^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:const\s+|async\s+|unsafe\s+|extern\s+"C"\s+)*fn\s+(\w+)', src, re.M):
        lineno = src[:m.start()].count('\n')
        # waiver: a comment line containing LOC-WAIVER within the
        # 3 lines above the fn (attrs may sit between)
        waived = any(
            WAIVER.search(lines[j])
            for j in range(max(0, lineno - 3), lineno)
        )
        depth = 0; opened = False; body = 0
        decl = False; pdepth = 0
        for i in range(lineno, min(lineno + 460, len(lines))):
            l = strip_braces_in_literals(lines[i])
            if not opened:
                # Signature scan: a top-level `;` before any `{` means a
                # bodyless declaration (`unsafe extern "C"` item) — skip.
                # `;` inside () / [] (array types like `[u8; 4]`) doesn't
                # terminate the signature.
                for ch in l:
                    if ch in '([':
                        pdepth += 1
                    elif ch in ')]':
                        pdepth -= 1
                    elif ch == '{':
                        break
                    elif ch == ';' and pdepth <= 0:
                        decl = True
                        break
                if decl:
                    break
            depth += l.count('{') - l.count('}')
            if '{' in l:
                opened = True
            body = i - lineno + 1
            if opened and depth <= 0:
                break
        if opened and body > 50 and not waived:
            FAIL.append(f"fn>50: {f}::{m.group(2)} ({body})")

if FAIL:
    print("locgate: FAIL")
    for x in FAIL:
        print("  " + x)
    print(f"locgate: {len(FAIL)} violation(s). Split, or waive a pure")
    print("data-driven dispatch/match table with a '// LOC-WAIVER:' line.")
    sys.exit(1)
print("locgate: PASS (files ≤500, fns ≤50 or waivered)")
PY
