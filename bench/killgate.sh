#!/bin/bash
# killgate — the mechanical form of hard rule 1/2 in
# bench/INCIDENT-2026-07-perfgate-pkill-massacre.md.
#
# `pkill -f "$X"` with an empty $X matches EVERY process on the box; as
# root that SIGTERMs sshd/systemd and the machine goes dark. That is not
# hypothetical — it took lx64 offline three times (2026-07-12/17/18).
# The lesson was written down after an earlier, milder incident and then
# ignored by a script written later. A note in a doc does not survive; a
# gate does.
#
# The rule, mechanically: every `pkill -f` whose pattern interpolates a
# variable must have an emptiness guard on that variable within the three
# preceding lines. Patterns with no `$` are bounded by construction and
# pass. `pgrep` is read-only and is not gated.
#
# Usage: bash bench/killgate.sh [dir ...]     (default: bench scripts/)
set -euo pipefail
cd "$(dirname "$0")/.."
DIRS=${*:-"bench scripts"}

python3 - $DIRS <<'PYEOF'
import re, sys, pathlib

# A pkill invocation carrying -f (match against full cmdline).
PKILL_F = re.compile(r'\bpkill\b(?=[^\n|;&]*\s-\w*f)')
# Variables named inside the kill pattern: $VAR, ${VAR}, ${VAR:-...}
VAR = re.compile(r'\$\{?([A-Za-z_][A-Za-z0-9_]*)')
# An emptiness guard for a specific variable, in any of the shapes the
# repo uses: [ -n "$V" ], [ -n "${V:-}" ], test -n "$V".
def guarded(var, window):
    pat = re.compile(r'-n\s+"?\$\{?' + re.escape(var) + r'(\}|:-\}|")')
    return any(pat.search(l) for l in window)

bad = []
files = []
for d in sys.argv[1:]:
    p = pathlib.Path(d)
    if p.is_dir():
        files += sorted(p.rglob('*.sh'))

for f in files:
    lines = f.read_text(errors='replace').splitlines()
    for i, line in enumerate(lines):
        code = line.split('#', 1)[0]
        if not PKILL_F.search(code):
            continue
        vars_used = set(VAR.findall(code))
        if not vars_used:
            continue                      # literal pattern — bounded
        window = lines[max(0, i - 3):i + 1]
        missing = [v for v in vars_used if not guarded(v, window)]
        if missing:
            bad.append((f, i + 1, line.strip(), missing))

if bad:
    print("killgate: FAIL — unguarded interpolated `pkill -f`", file=sys.stderr)
    print("  An empty variable makes the pattern match every process on the",
          file=sys.stderr)
    print("  box. Guard it, or kill a captured PID instead (see",
          file=sys.stderr)
    print("  bench/INCIDENT-2026-07-perfgate-pkill-massacre.md).\n", file=sys.stderr)
    for f, n, line, missing in bad:
        print(f"  {f}:{n}: {line}", file=sys.stderr)
        print(f"      no `[ -n \"${{{missing[0]}}}\" ]` guard within 3 lines above",
              file=sys.stderr)
    sys.exit(1)

print(f"killgate: OK — {len(files)} shell scripts, every interpolated "
      f"`pkill -f` guarded")
PYEOF
