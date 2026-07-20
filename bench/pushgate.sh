#!/usr/bin/env bash
# pushgate — run the CI checks that need no server, before pushing.
#
# Why this exists: the local command set had drifted from the CI command
# set, so each round came back red on a different gate — and one local
# invocation (`cargo clippy --all-features`) was not the one CI runs at
# all. Rounds spent discovering that are rounds not spent on the defect.
#
# What it deliberately does NOT do is claim to be CI. It runs one tier of
# checks and then PRINTS EVERY CI STEP IT DOES NOT COVER, so a green
# pushgate can never be mistaken for a green build. A gate that silently
# covers a subset is the same shape of trap as a durability guarantee
# with an invisible size cliff.
#
# Usage:
#   bash bench/pushgate.sh               # lint tier (~2 min)
#   bash bench/pushgate.sh --with-tests  # + workspace build and tests
set -u
cd "$(dirname "$0")/.."

WITH_TESTS=0
[ "${1:-}" = "--with-tests" ] && WITH_TESTS=1

FAILED=()
PASSED=0

run() { # run <label> <command...>
    local label="$1"; shift
    printf '\n\033[1m== %s\033[0m\n' "$label"
    if "$@"; then
        PASSED=$((PASSED + 1))
    else
        FAILED+=("$label")
    fi
}

# ---- lint tier: the checks that have actually been failing -----------
# Clippy's flags mirror the CI step exactly. Note there is no
# --all-features: kevy-client-async's runtime features are mutually
# exclusive by design and enabling them together is a hard error.
run "clippy"      cargo clippy --workspace --all-targets -- -D warnings
run "locgate"     bash bench/locgate.sh
run "commentgate" bash bench/commentgate.sh
run "killgate"    bash bench/killgate.sh
run "vendorgate"  bash bench/vendorgate.sh
run "docs parity" cargo run -q -p kevy --bin gen_docs -- . --check
run "command pages parity" python3 tools/gen_command_pages.py --check
run "CJK punctuation"      python3 tools/check_cjk_punct.py
run "site links"           python3 tools/check_links.py

if [ "$WITH_TESTS" = 1 ]; then
    run "build"  cargo build --workspace
    run "test"   cargo test --workspace --lib --tests
fi

# ---- what this run did NOT cover ------------------------------------
# Derived from ci.yml rather than hand-listed, so a newly added CI step
# shows up here as uncovered instead of being silently missed.
printf '\n\033[1m== CI steps NOT covered by this run\033[0m\n'
COVERED_KEYS="clippy locgate.sh commentgate.sh killgate.sh vendorgate.sh gen_docs gen_command_pages check_cjk_punct check_links"
[ "$WITH_TESTS" = 1 ] && COVERED_KEYS="$COVERED_KEYS cargo build --workspace|cargo test --workspace"

COVERED_KEYS="$COVERED_KEYS" python3 - <<'PY'
import os, re
covered = os.environ['COVERED_KEYS'].split()
name = None
uncovered = []
for line in open('.github/workflows/ci.yml'):
    m = re.match(r'\s+- name: (.*)', line)
    if m:
        name = m.group(1).strip()
        continue
    m = re.match(r'\s+run: (.+)', line)
    if m and name:
        cmd = m.group(1)
        if not any(k in cmd for k in covered):
            uncovered.append(name)
        name = None
seen = set()
for n in uncovered:
    if n in seen or n.startswith(('Install', 'Cache')):
        continue
    seen.add(n)
    print(f"  - {n}")
PY

printf '\n'
if [ ${#FAILED[@]} -eq 0 ]; then
    echo "pushgate: PASS ($PASSED checks) — the steps listed above still only run in CI"
    exit 0
fi
echo "pushgate: FAIL — ${FAILED[*]}"
exit 1
