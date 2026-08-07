#!/usr/bin/env bash
# funcgate — the scalar-function surface against the 89-probe corpus.
#
# Two assertions (RFC 2026-08-08 §4):
#   1. wrong == 0 — a silent wrong answer fails the gate outright.
#      Refusing by name is honest; answering wrong is not.
#   2. subset-foldable ratio >= FLOOR — a RATCHET at the measured
#      level (72% at introduction), pending the owner's bar decision
#      (RFC 拍板点①). Raise it as functions land; never lower it.
#
# Deterministic: the corpus clock is pinned inside `sql probe`, no
# server, no network — CI-safe.
set -euo pipefail
cd "$(dirname "$0")/.."
FLOOR=${FUNCGATE_FLOOR:-72}

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
