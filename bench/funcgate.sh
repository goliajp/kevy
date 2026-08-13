#!/usr/bin/env bash
# funcgate — the scalar-function surface against the 89-probe corpus.
#
# Two assertions (RFC 2026-08-08 §4):
#   1. wrong == 0 — a silent wrong answer fails the gate outright.
#      Refusing by name is honest; answering wrong is not.
#   2. subset-foldable ratio >= FLOOR — a RATCHET at the measured level,
#      ratcheting up only when a real improvement lands (perfgate's
#      philosophy). The charter's original "89 probes x 80% served" was
#      written before anyone counted: its denominator swallowed the 52
#      probes this arc permanently refuses by name (catalog emulation,
#      relational algebra, exotic types), which caps the ratio near 50%
#      and would have forced a bar nobody could pass honestly. The
#      resolved bar is two lines instead — capability here, honesty in
#      rule 1 — see the RFC's Resolution section.
#      (RFC 拍板点①). Raise it as functions land; never lower it.
#
# Deterministic: the corpus clock is pinned inside `sql probe`, no
# server, no network — CI-safe.
set -euo pipefail
cd "$(dirname "$0")/.."
FLOOR=${FUNCGATE_FLOOR:-82}

OUT=$(cargo run -q -p kevy-cli -- sql probe bench/funcgate-corpus)
echo "$OUT" | tail -20
SUMMARY=$(echo "$OUT" | grep '^probe-summary:')

WRONG=$(echo "$SUMMARY" | grep -oE 'wrong=[0-9]+' | cut -d= -f2)
PCT=$(echo "$SUMMARY" | grep -oE 'subset-foldable=[0-9]+/[0-9]+ \([0-9.]+' | grep -oE '[0-9.]+$')

if [ "${WRONG:-1}" != "0" ]; then
    echo "funcgate: FAIL — $WRONG silent wrong answer(s); refusals are honest, wrong answers are not"
    exit 1
fi
if ! awk -v p="$PCT" -v f="$FLOOR" 'BEGIN{exit !(p >= f)}'; then
    echo "funcgate: FAIL — subset-foldable ${PCT}% under the ${FLOOR}% ratchet"
    exit 1
fi
echo "funcgate: PASS (wrong=0, subset-foldable ${PCT}% >= ${FLOOR}%)"
