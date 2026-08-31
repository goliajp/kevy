#!/bin/bash
# killgate — the mechanical form of hard rules 1 and 2 for pkill.
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
# pass.
#
# Second rule, added 2026-08-13: a WAIT loop must not decide whether its
# subject is alive by looking for the subject's own name. `until ! pgrep -f
# arena.sh` never exits, because the shell running that pgrep has
# `arena.sh` in its own command line and pgrep counts it. That is the
# read-only face of the same confusion the incident above is the
# destructive face of, and it costs a background slot and an hour rather
# than a machine — which is why it went unnoticed twice. See
# the hygiene rule that a wait condition must not match itself.
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
    print("  box. Guard it, or kill a captured PID instead.\n",
          file=sys.stderr)
    for f, n, line, missing in bad:
        print(f"  {f}:{n}: {line}", file=sys.stderr)
        print(f"      no `[ -n \"${{{missing[0]}}}\" ]` guard within 3 lines above",
              file=sys.stderr)
    sys.exit(1)

# ── wait loops must not self-match ──────────────────────────────────────
# A `pgrep`/`pkill` pattern inside a `until`/`while` condition, where the
# pattern is a plain script name, matches the very shell evaluating it.
selfmatch = []
for f in files:
    lines = pathlib.Path(f).read_text(errors="replace").split("\n")
    for i, line in enumerate(lines):
        if not re.search(r"\b(until|while)\b", line):
            continue
        m = re.search(r"pgrep\s+(-\w+\s+)*-\w*f\w*\s+['\"]?([\w./-]+)['\"]?", line)
        if not m:
            continue
        pat = m.group(2)
        # `pgrep -x` matches the executable name exactly and cannot match a
        # shell whose ARGUMENTS mention it; a pattern anchored with ^ or
        # carrying a path separator is likewise specific enough.
        if "-x" in line or pat.startswith("^") or "\\." in pat:
            continue
        selfmatch.append((f, i + 1, line.strip()))

if selfmatch:
    print("killgate: FAIL — a wait loop that matches itself", file=sys.stderr)
    print("  `until ! pgrep -f <name>` never exits: the shell running that",
          file=sys.stderr)
    print("  pgrep has <name> in its own command line, so pgrep counts it.",
          file=sys.stderr)
    print("  Wait on evidence the subject produces instead — a finished-marker",
          file=sys.stderr)
    print("  line, a flag file, an exit-code file.\n",
          file=sys.stderr)
    for f, n, line in selfmatch:
        print(f"  {f}:{n}: {line}", file=sys.stderr)
    sys.exit(1)

print(f"killgate: OK — {len(files)} shell scripts, every interpolated "
      f"`pkill -f` guarded, no self-matching wait loop")
PYEOF
