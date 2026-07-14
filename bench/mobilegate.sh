#!/usr/bin/env bash
# mobilegate — the mobile doors open on a real device, gated.
#
# Drives the expo-kevy example app onto an iOS simulator and/or an
# Android emulator, then reads the on-screen smoke verdict the app paints
# on mount (App.tsx runs SET/GET, TTL, INCRBY, reopen-durability and
# version, and renders "ALL PASS" only if every check held). The same
# assertions are a human demo and this gate.
#
# Because a full native build + boot is heavy and toolchain-bound, this
# is a DEVELOPER/CI-on-macOS gate, not part of the per-push matrix. Run:
#
#   bash bench/mobilegate.sh ios       # iOS simulator
#   bash bench/mobilegate.sh android   # Android emulator
#   bash bench/mobilegate.sh both
#
# Prerequisites: Xcode + an iOS simulator (ios), Android SDK + an AVD
# (android), and the native artifacts vendored:
#   packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
#   packaging/android/build-jnilibs.sh
#   (cd bindings/expo && bash scripts/prepare-native.sh)
set -euo pipefail
cd "$(dirname "$0")/../bindings/expo/example"

if ! command -v npx >/dev/null 2>&1; then
    nvm_bin=$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -1)
    [ -n "$nvm_bin" ] && export PATH="$nvm_bin:$PATH"
fi

target=${1:-ios}
[ -d node_modules ] || npm install --no-audit --no-fund

# App.tsx logs "MOBILEGATE:<PASS|FAIL|ERROR>" once its on-mount smoke
# settles. We capture that from the DEVICE's own log stream, not from the
# expo CLI's stdout: `expo run` exits after launching the app (it does
# not reliably stay attached streaming the JS console), so the verdict
# would be missed. The device log stream is started BEFORE launch and
# outlives the CLI, so the line is caught whenever the app prints it.
run_ios() {
    local logf; logf=$(mktemp)
    echo "mobilegate: streaming the simulator log…"
    xcrun simctl spawn booted log stream --level debug \
        --predicate 'eventMessage CONTAINS "MOBILEGATE"' > "$logf" 2>/dev/null &
    local streampid=$!
    echo "mobilegate: building + booting on ios (this is slow)…"
    local runf; runf=$(mktemp)
    ( cd . && npx expo run:ios > "$runf" 2>&1 ) &
    local runpid=$!
    local rc=1
    for _ in $(seq 360); do # up to ~12 min for cold build + boot + smoke
        if grep -q "MOBILEGATE:PASS" "$logf"; then echo "mobilegate: ios PASS"; rc=0; break; fi
        if grep -qE "MOBILEGATE:(FAIL|ERROR)" "$logf"; then
            echo "mobilegate: ios FAIL"; grep "MOBILEGATE:" "$logf" | head -1; rc=1; break; fi
        if ! kill -0 $runpid 2>/dev/null && grep -qE "error|Error:" "$runf" \
           && ! grep -q "Build Succeeded" "$runf"; then
            echo "mobilegate: ios build failed"; tail -15 "$runf"; rc=1; break; fi
        sleep 2
    done
    kill $streampid $runpid 2>/dev/null || true
    return $rc
}

run_one() {
    local platform=$1
    if [ "$platform" = ios ]; then run_ios; return $?; fi
    # Android: expo run:android streams logcat through the CLI reliably.
    local logf; logf=$(mktemp)
    echo "mobilegate: building + booting on android (this is slow)…"
    npx expo run:android 2>&1 | tee "$logf" &
    local pid=$!
    for _ in $(seq 360); do
        if grep -q "MOBILEGATE:PASS" "$logf"; then
            echo "mobilegate: android PASS"; kill $pid 2>/dev/null || true; return 0; fi
        if grep -qE "MOBILEGATE:(FAIL|ERROR)" "$logf"; then
            echo "mobilegate: android FAIL"; grep "MOBILEGATE:" "$logf" | head -1; kill $pid 2>/dev/null || true; return 1; fi
        kill -0 $pid 2>/dev/null || { echo "mobilegate: android exited early"; tail -20 "$logf"; return 1; }
        sleep 2
    done
    echo "mobilegate: android timed out"; kill $pid 2>/dev/null || true; return 1
}

rc=0
case "$target" in
    ios) run_one ios || rc=1 ;;
    android) run_one android || rc=1 ;;
    both) run_one ios || rc=1; run_one android || rc=1 ;;
    *) echo "usage: mobilegate.sh ios|android|both" >&2; exit 2 ;;
esac
[ $rc = 0 ] && echo "mobilegate: PASS ($target)" || echo "mobilegate: FAIL ($target)"
exit $rc
