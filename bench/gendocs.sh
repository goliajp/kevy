#!/usr/bin/env bash
# gen-docs — every document derived from the engine's verb table says what
# the table says.
#
#   - llms.txt and the verb reference are what gen_docs generates today;
#   - the command count the site publishes is the count the engine answers;
#   - the READMEs quote the verb count the three-way differential runs;
#   - the site's command reference is the engine's own COMMAND DOCS.
#
# All four run, and the gate reports every one that disagrees rather than
# stopping at the first.
#
# usage: bench/gendocs.sh        (needs target/debug/kevy)
set -u
cd "$(dirname "$0")/.." || exit 2

fail=0
run() {
  "$@" || { echo "gen-docs: FAIL — $*"; fail=1; }
}

run cargo run -q -p kevy --bin gen_docs -- . --check
run python3 tools/check_capability_claim.py
run python3 tools/check_compat_claim.py
run python3 tools/export_site_commands.py --check

[ "$fail" = 0 ] && echo "gen-docs: PASS"
exit "$fail"
