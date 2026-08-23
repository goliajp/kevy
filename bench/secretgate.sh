#!/usr/bin/env bash
# secretgate — a credential must never be stageable, and never tracked.
#
# Ignoring a file is not the same as protecting it. During the 5.4.0 release
# the rule that ignored `.env.local` lived in `.gitignore`, which is itself
# tracked: a `git stash` on that file took the protection away and left the
# credential behind, visible to `git status`. The rule now lives in
# `.git/info/exclude`, which no working-tree operation moves — and this gate
# checks the outcome rather than the rule, because a rule can be moved again.
#
# Two things it refuses:
#   1. anything matching a credential name is tracked, or staged (`git add -f`
#      defeats every ignore file there is);
#   2. a token-shaped literal appears in a tracked file.
set -euo pipefail
cd "$(dirname "$0")/.."

PATTERNS='(^|/)\.env($|\.)|(^|/)\.npmrc$|(^|/)id_(rsa|ed25519)$|(^|/)\.pypirc$'

fail=0

tracked=$(git ls-files | grep -iE "$PATTERNS" || true)
if [ -n "$tracked" ]; then
  echo "secretgate: FAIL — these credential files are TRACKED:"
  printf '  %s\n' $tracked
  fail=1
fi

staged=$(git diff --cached --name-only | grep -iE "$PATTERNS" || true)
if [ -n "$staged" ]; then
  echo "secretgate: FAIL — these credential files are STAGED:"
  printf '  %s\n' $staged
  fail=1
fi

# Token literals in tracked content. The shapes are the ones this project
# actually handles: npm automation tokens, crates.io, GitHub PATs.
# `git grep` searches tracked files only, which is the point.
leak=$(git grep -nIE '(npm_[A-Za-z0-9]{36}|cio[A-Za-z0-9]{32}|gh[pousr]_[A-Za-z0-9]{36})' \
       -- . ':!bench/secretgate.sh' || true)
if [ -n "$leak" ]; then
  echo "secretgate: FAIL — a token-shaped literal is in tracked content:"
  printf '  %s\n' "$leak" | head -5
  fail=1
fi

# A gate that cannot see the repository is not a passing gate.
n=$(git ls-files | wc -l | tr -d ' ')
[ "$n" -gt 100 ] || {
  echo "secretgate: REFUSED — git ls-files returned $n paths; this is not the tree." >&2
  exit 2
}

[ $fail -eq 0 ] || exit 1
echo "secretgate: PASS — no credential file tracked or staged, no token literal in $n files"
