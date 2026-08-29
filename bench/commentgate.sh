#!/usr/bin/env bash
# commentgate — no internal vocabulary in source comments (v4 T1c).
#
# Comments describe the semantics and constraints of the code in front
# of the reader; history (which version, which batch, which plan) lives
# in git log and the CHANGELOG. This gate scans the comment text of
# every non-test src file against bench/commentgate-terms.txt (the same
# table the T2 sweep works from) and fails on any hit.
#
# Usage:
#   bash bench/commentgate.sh          # gate: per-crate table, exit 1 on hits
#   bash bench/commentgate.sh --list   # also print every hit (file:line: text)
set -u
cd "$(dirname "$0")/.."

LIST="${1:-}"

LIST_MODE="$LIST" python3 - <<'PY'
import os, re, glob, sys
from collections import Counter

list_mode = os.environ.get('LIST_MODE') == '--list'

# ---- load the term table -------------------------------------------
deny, allow = [], []
for raw in open('bench/commentgate-terms.txt', encoding='utf-8'):
    line = raw.strip()
    if not line or line.startswith('#'):
        continue
    kind, _, pat = line.partition(':')
    if kind == 'deny':
        deny.append(re.compile(pat))
    elif kind == 'allow':
        allow.append(re.compile(pat))
    else:
        sys.exit(f"commentgate: bad directive in terms file: {line!r}")

# ---- file set: crates/*/src, tests exempt (same shape as locgate) ---
files = sorted(set(glob.glob('crates/*/src/**/*.rs', recursive=True)))
def exempt_file(f):
    name = f.split('/')[-1]
    return '/tests' in f or name.endswith('_tests.rs') or name.startswith('tests_')

# ---- comment extraction --------------------------------------------
# Strip string literals first so `//` inside a URL string or a semver
# inside an INFO reply string never reaches the scanner. Handles plain
# strings with escapes, raw strings r"…"/r#"…"#, and char literals
# (so `'"'` can't derail the string stripper).
STR = re.compile(
    r'''r\#+".*?"\#+   # raw hashed string (single line)
      | r".*?"         # raw string
      | "(?:\\.|[^"\\])*"   # plain string with escapes
      | '(?:\\.|[^'\\])'    # char literal
    ''', re.X)

def comment_text(line):
    """Return the comment portion of a source line, or None."""
    s = STR.sub('""', line)
    i = s.find('//')
    if i < 0:
        return None
    # Positions map 1:1 only up to the first stripped literal; the
    # comment text itself must come from the stripped line so string
    # contents can't leak in.
    return s[i:]

# ---- scan -----------------------------------------------------------
hits = []                 # (file, lineno, term, text)
per_crate = Counter()
per_term = Counter()

for f in files:
    if exempt_file(f):
        continue
    crate = f.split('/')[1]
    for n, line in enumerate(open(f, errors='replace'), 1):
        c = comment_text(line.rstrip('\n'))
        if c is None:
            continue
        for a in allow:
            c = a.sub(lambda m: ' ' * len(m.group(0)), c)
        for d in deny:
            m = d.search(c)
            if m:
                hits.append((f, n, m.group(0), c.strip()))
                per_crate[crate] += 1
                per_term[d.pattern] += 1

# ---- report ----------------------------------------------------------
# Same tree locgate scans, same floor, same reason: zero hits across zero
# files reads exactly like zero hits across three hundred.
MIN_FILES = 100
if len(files) < MIN_FILES:
    print(f"commentgate: REFUSED — scanned {len(files)} source file(s), "
          f"expected at least {MIN_FILES}. Zero hits over an empty set is "
          f"not a clean bill of health.")
    sys.exit(2)
if not hits:
    print(f"commentgate: PASS (0 internal-vocabulary hits across {len(files)} files)")
    sys.exit(0)

print(f"commentgate: FAIL — {len(hits)} hit(s)\n")
print("per-crate:")
for crate, n in per_crate.most_common():
    print(f"  {n:6}  {crate}")
print("\nper-pattern:")
for pat, n in per_term.most_common():
    print(f"  {n:6}  {pat}")
if list_mode:
    print("\nhits:")
    for f, n, term, text in hits:
        print(f"  {f}:{n}: [{term}] {text}")
else:
    print("\n(run with --list for file:line detail)")
sys.exit(1)
PY
