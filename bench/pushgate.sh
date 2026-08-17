#!/usr/bin/env bash
# pushgate — the precommit tier, under its old habit-name.
#
# The suite (suite/manifest.toml, tools/suite.py) is the single source
# of truth since 5.3: three tiers, audited budgets, loud NOT-RUN rows.
# This wrapper exists because "bash bench/pushgate.sh before pushing"
# is muscle memory worth keeping; what it runs is exactly
#
#   python3 tools/suite.py precommit
#
# and --with-tests adds the workspace test run from the prerelease tier.
# Everything pushgate used to print about uncovered CI steps is now the
# suite's own accounting: the manifest audit fails if a check's file
# vanishes, and the verdict names every NOT-RUN row.
set -u
cd "$(dirname "$0")/.."

python3 tools/suite.py precommit || exit 1
if [ "${1:-}" = "--with-tests" ]; then
    python3 tools/suite.py prerelease --only workspace-tests || exit 1
fi
