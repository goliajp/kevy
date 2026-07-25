#!/usr/bin/env bash
# ffigate-contract — one contract table, six doors, checked.
#
# ffigate runs each language door's smoke on every push, and the CI comment
# says they "all run the same contract". They did not. The C++ wrapper had
# no scalar GET/SET at all — a C++ caller could not reach the fast path
# without dropping to the C API — and the Bun door never exercised the
# scalars its binding exposes. Both went unnoticed because the contract
# lived in a sentence, and a sentence cannot be run.
#
# The table below IS the contract: five rows, six doors, one regex per
# cell. A door that stops covering a row fails here, and a door that
# renames its API fails here too — which is correct, because that is
# exactly when the table needs a human.
#
# What this checks is that the assertion is PRESENT. That it PASSES is
# ffigate's job, which runs these same smokes in CI.
#
#   bash bench/ffigate-contract.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE" || exit 1

# Sources per door. A door's contract may be spread across its files (Go
# keeps error-as-data in kevy_test.go and durability in embedded_test.go),
# so each door is searched as a set.
doors="c cpp go bun node csharp"
src_c="crates/kevy-ffi/examples/c/smoke.c"
src_cpp="crates/kevy-ffi/examples/cpp/smoke.cpp"
src_go="bindings/go/embedded_test.go bindings/go/kevy_test.go"
src_bun="bindings/node/bun.test.js"
src_node="bindings/node/node.test.js"
src_csharp="bindings/csharp/smoke/Program.cs"

# The table. rows × doors, one extended regex per cell.
rows="cmd error-as-data pubsub durability scalar"

cmd_c='kevy_cmd\('              ; cmd_cpp='db\.cmd\('
cmd_go='db\.Cmd\('              ; cmd_bun='db\.cmd\('
cmd_node='db\.cmd\('            ; cmd_csharp='db\.Cmd\('

error_as_data_c="err\.ptr\[0\] == '-'"      ; error_as_data_cpp='is_error\(\)'
error_as_data_go='IsError\(\)'              ; error_as_data_bun='instanceof KevyError|toBeInstanceOf\(KevyError'
error_as_data_node='instanceof KevyError'   ; error_as_data_csharp='\.IsError'

pubsub_c='kevy_subscribe\('     ; pubsub_cpp='db\.subscribe\('
pubsub_go='db\.Subscribe\('     ; pubsub_bun='db\.subscribe\('
pubsub_node='db\.subscribe\('   ; pubsub_csharp='db\.Subscribe\('

durability_c='kevy_close\(db\)' ; durability_cpp='reopen'
durability_go='Reopen'          ; durability_bun='db\.close\(\)'
durability_node='db\.close\(\)' ; durability_csharp='reopen|Reopen'

scalar_c='kevy_get\('           ; scalar_cpp='db\.get\('
scalar_go='c2?\.(Get|Set)\(bg'  ; scalar_bun='getScalar\('
scalar_node='db\.get\('         ; scalar_csharp='db\.GetText\('

fail=0
checked=0
printf '%-16s' "contract"
for d in $doors; do printf '%-8s' "$d"; done
echo
for row in $rows; do
    key="${row//-/_}"
    printf '%-16s' "$row"
    for d in $doors; do
        eval "pat=\${${key}_${d}:-}"
        eval "files=\${src_${d}}"
        if [ -z "$pat" ]; then
            printf '%-8s' "NOPAT"; fail=1; continue
        fi
        checked=$((checked + 1))
        # shellcheck disable=SC2086
        if grep -qE "$pat" $files 2>/dev/null; then
            printf '%-8s' "ok"
        else
            printf '%-8s' "MISS"; fail=1
        fi
    done
    echo
done

if [ "$fail" -ne 0 ]; then
    echo
    echo "ffigate-contract: FAIL — a door is missing a contract row, or its"
    echo "  API was renamed out from under the table. Either cover the row in"
    echo "  that door's smoke, or update the cell in this script."
    exit 1
fi
echo
ndoors=$(echo "$doors" | wc -w | tr -d " ")
nrows=$(echo "$rows" | wc -w | tr -d " ")
echo "ffigate-contract: PASS — $checked cells, $ndoors doors × $nrows rows"
