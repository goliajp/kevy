#!/usr/bin/env bash
# mobilegate-all — the phase-2 summary: three frameworks x two platforms,
# one table, one exit code.
#
# mobilegate runs one framework/platform per invocation. This drives all
# six, captures each verdict, and prints a grid — so "all three frameworks
# green on both platforms" is a single command with a single answer, not
# six runs a human has to remember the results of.
#
# Each cell is a full native build + device boot, so this is slow (tens of
# minutes) and, like mobilegate itself, a developer/CI-on-macOS gate, not
# part of the per-push matrix. Both a booted iOS simulator and a booted
# Android emulator must be present; ANDROID_SERIAL should pin the emulator
# when a physical phone is also attached (mobilegate installs to it
# otherwise — see that script's header).
#
#   ANDROID_SERIAL=emulator-5554 bash bench/mobilegate-all.sh
#   FRAMEWORKS="flutter expo" bash bench/mobilegate-all.sh   # a subset
#
# Exit 0 only if every selected cell PASSed.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

frameworks=${FRAMEWORKS:-expo barern flutter}
platforms=${PLATFORMS:-ios android}

declare -A result
overall=0
for fw in $frameworks; do
    for plat in $platforms; do
        echo "════ mobilegate $fw/$plat ════"
        if bash "$HERE/mobilegate.sh" "$fw" "$plat"; then
            result["$fw/$plat"]=PASS
        else
            result["$fw/$plat"]=FAIL
            overall=1
        fi
    done
done

echo
echo "mobilegate-all — three frameworks x two platforms"
printf '%-10s' ""
for plat in $platforms; do printf '%-10s' "$plat"; done
echo
for fw in $frameworks; do
    printf '%-10s' "$fw"
    for plat in $platforms; do
        printf '%-10s' "${result["$fw/$plat"]:-—}"
    done
    echo
done
echo
if [ "$overall" -eq 0 ]; then
    echo "mobilegate-all: PASS — every selected cell green"
else
    echo "mobilegate-all: FAIL — a cell above is not PASS"
fi
exit "$overall"
