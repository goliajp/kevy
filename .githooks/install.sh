#!/usr/bin/env bash
# One-time per-clone developer setup. Wires the hooks + git-flow
# defaults that aren't shareable via `.git/config` (which never enters
# the repo). Re-run if `.githooks/` or this script changes.
#
#   bash .githooks/install.sh
#
# Idempotent — re-runs leave the config in the same state.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# 1. Pre-commit (and any future hooks) come from .githooks/ instead of
#    the per-clone .git/hooks/. Now committable, reviewable, shared.
git config core.hooksPath .githooks
echo "✓ core.hooksPath = .githooks"

# 2. `git flow feature finish` squashes the feature's commits into one
#    landing on develop (see GIT-FLOW.md for the rationale).
#    Sub-commits stay reachable via the feature branch's reflog until
#    git GCs them.
git config gitflow.feature.finish.squash true
echo "✓ gitflow.feature.finish.squash = true (squash-merge feature → develop)"

# 3. `git flow feature finish` and `git flow release finish` fetch
#    from origin first — saves a "your branch is behind" surprise.
git config gitflow.feature.finish.fetch true
git config gitflow.release.finish.fetch true
echo "✓ gitflow.{feature,release}.finish.fetch = true"

echo ""
echo "Done. Pre-commit will now enforce the 500-LOC src/*.rs cap;"
echo "feature finishes will squash; finishes will fetch first."
