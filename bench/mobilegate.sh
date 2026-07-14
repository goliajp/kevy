#!/usr/bin/env bash
# mobilegate — the mobile doors open on a real device, gated.
#
# Drives a kevy example app onto an iOS simulator or an Android emulator,
# then reads the on-device smoke verdict the app logs on start
# (SET/GET, TTL, INCRBY, reopen-durability, version → MOBILEGATE:PASS).
# The verdict is captured from the DEVICE's own log (simctl log stream /
# adb logcat), not the build CLI, which exits after launch.
#
# Because a full native build + boot is heavy and toolchain-bound, this
# is a DEVELOPER/CI-on-macOS gate, not part of the per-push matrix. Run:
#
#   bash bench/mobilegate.sh expo ios          bash bench/mobilegate.sh flutter android
#   bash bench/mobilegate.sh expo android      bash bench/mobilegate.sh flutter ios
#
# Prereqs: Xcode + a booted iOS simulator (ios) / Android SDK + a booted
# emulator (android), and the door's native artifacts vendored (each
# door's scripts/prepare-native.sh).
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"

framework=${1:?usage: mobilegate.sh <expo|flutter> <ios|android>}
platform=${2:?usage: mobilegate.sh <expo|flutter> <ios|android>}

if ! command -v npx >/dev/null 2>&1; then
    nvm_bin=$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -1)
    [ -n "$nvm_bin" ] && export PATH="$nvm_bin:$PATH"
fi
command -v flutter >/dev/null 2>&1 || export PATH="/opt/homebrew/bin:$PATH"

ios_sim_id() {
    xcrun simctl list devices booted -j | grep -o '"udid" : "[^"]*"' | head -1 | sed 's/.*: "//;s/"//'
}
android_dev_id() { adb devices | grep -w device | grep -v List | head -1 | cut -f1; }

case "$framework" in
    expo)
        appdir="$HERE/bindings/expo/example"
        [ -d "$appdir/node_modules" ] || ( cd "$appdir" && npm install --no-audit --no-fund )
        ios_cmd="npx expo run:ios"
        android_cmd="npx expo run:android"
        ;;
    flutter)
        appdir="$HERE/bindings/flutter/example"
        # iOS simulators run debug only (release/profile need a device);
        # the Android emulator runs release fine.
        ios_cmd="flutter run --debug -d $(ios_sim_id)"
        android_cmd="flutter run --release -d $(android_dev_id)"
        ;;
    *) echo "unknown framework: $framework" >&2; exit 2 ;;
esac

# Capture the verdict from the device log, run the build, gate the line.
run() {
    local build="$1" streamcmd="$2" logf runf rc=1
    logf=$(mktemp); runf=$(mktemp)
    echo "mobilegate: streaming device log…"
    eval "$streamcmd" > "$logf" 2>/dev/null &
    local streampid=$!
    echo "mobilegate: building + booting $framework/$platform (this is slow)…"
    ( cd "$appdir" && eval "$build" > "$runf" 2>&1 ) &
    local runpid=$!
    for _ in $(seq 480); do
        if grep -q "MOBILEGATE:PASS" "$logf"; then echo "mobilegate: $framework/$platform PASS"; rc=0; break; fi
        if grep -qE "MOBILEGATE:(FAIL|ERROR)" "$logf"; then
            echo "mobilegate: $framework/$platform FAIL"; grep "MOBILEGATE" "$logf" | head -1; rc=1; break; fi
        if ! kill -0 $runpid 2>/dev/null && grep -qiE "FAILURE|error:|✖|❌|Exception" "$runf" \
           && ! grep -qiE "Build Succeeded|BUILD SUCCESSFUL|Syncing files|Installing" "$runf"; then
            echo "mobilegate: $framework/$platform build failed"; tail -20 "$runf"; rc=1; break; fi
        sleep 2
    done
    kill $streampid $runpid 2>/dev/null || true
    return $rc
}

case "$platform" in
    ios)
        run "$ios_cmd" "xcrun simctl spawn booted log stream --level debug --predicate 'eventMessage CONTAINS \"MOBILEGATE\"'"
        ;;
    android)
        adb logcat -c 2>/dev/null || true
        run "$android_cmd" "adb logcat | grep --line-buffered MOBILEGATE"
        ;;
    *) echo "unknown platform: $platform" >&2; exit 2 ;;
esac
