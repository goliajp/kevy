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
#   bash bench/mobilegate.sh barern android    # expo-kevy in a BARE RN app
#
# Prereqs: Xcode + a booted iOS simulator (ios) / Android SDK + a booted
# emulator (android), and the door's native artifacts vendored (each
# door's scripts/prepare-native.sh).
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"

framework=${1:?usage: mobilegate.sh <expo|flutter|barern> <ios|android>}
platform=${2:?usage: mobilegate.sh <expo|flutter|barern> <ios|android>}

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
        # Dedicated Metro port: the default 8081 collides with any other RN
        # project's Metro on a shared box, and the app then loads a FOREIGN
        # bundle (observed: a stray 8081 Metro fed another app's bundle and
        # crashed on its missing native modules).
        ios_cmd="npx expo run:ios --port 8087"
        android_cmd="npx expo run:android --port 8087"
        ;;
    flutter)
        appdir="$HERE/bindings/flutter/example"
        # iOS simulators run debug only (release/profile need a device);
        # the Android emulator runs release fine.
        ios_cmd="flutter run --debug -d $(ios_sim_id)"
        android_cmd="flutter run --release -d $(android_dev_id)"
        ;;
    barern)
        # expo-kevy in a BARE @react-native-community/cli app. Both the iOS
        # Podfile and the Android gradle are hand-wired for expo-modules
        # autolinking (RN 0.86 / SDK 57 — install-expo-modules is dead there;
        # the wiring mirrors `expo prebuild`). Release build bundles the JS so
        # no metro server is needed for the gate.
        #
        # iOS prereq (once): build + vendor the native artifact, then pods —
        #   packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
        #   bash bindings/expo/scripts/prepare-native.sh   # copies Kevy.xcframework in
        #   ( cd bindings/expo/barern-example/ios && pod install )
        # Built arm64-only: Kevy.xcframework ships an arm64 iOS-simulator slice
        # (Apple-Silicon host, per packaging/apple/build-xcframework.sh), so a
        # fat arm64+x86_64 Release build finds no matching sim slice. Restricting
        # to the active arm64 arch matches the slice (same as `expo run:ios`).
        appdir="$HERE/bindings/expo/barern-example"
        [ -d "$appdir/node_modules" ] || ( cd "$appdir" && npm install --no-audit --no-fund )
        ios_cmd='sim=$(ios_sim_id); \
          [ -d ios/BareKevy.xcworkspace ] || ( cd ios && pod install ); \
          xcodebuild -workspace ios/BareKevy.xcworkspace -scheme BareKevy \
            -configuration Release -sdk iphonesimulator -derivedDataPath ios/build \
            -destination "id=$sim" ONLY_ACTIVE_ARCH=YES ARCHS=arm64 EXCLUDED_ARCHS=x86_64 \
            CODE_SIGNING_ALLOWED=NO build && \
          app=$(find ios/build/Build/Products/Release-iphonesimulator -maxdepth 1 -name "*.app" | head -1) && \
          xcrun simctl install "$sim" "$app" && \
          xcrun simctl launch "$sim" org.reactjs.native.example.BareKevy'
        android_cmd="npx react-native run-android --mode release"
        ;;
    *) echo "unknown framework: $framework" >&2; exit 2 ;;
esac

# Capture the verdict from the device log, run the build, gate the line.
run() {
    local build="$1" streamcmd="$2" logf runf rc=1 verdict=""
    logf=$(mktemp); runf=$(mktemp)
    echo "mobilegate: streaming device log…"
    eval "$streamcmd" > "$logf" 2>/dev/null &
    local streampid=$!
    echo "mobilegate: building + booting $framework/$platform (this is slow)…"
    ( cd "$appdir" && eval "$build" > "$runf" 2>&1 ) &
    local runpid=$!
    for _ in $(seq 480); do
        if grep -q "MOBILEGATE:PASS" "$logf"; then echo "mobilegate: $framework/$platform PASS"; rc=0; verdict=y; break; fi
        if grep -qE "MOBILEGATE:(FAIL|ERROR)" "$logf"; then
            echo "mobilegate: $framework/$platform FAIL"; grep "MOBILEGATE" "$logf" | head -1; rc=1; verdict=y; break; fi
        # The app crashing before it can log a verdict would otherwise burn
        # the whole timeout in silence — fail fast on the crash line instead.
        if grep -qE "FATAL EXCEPTION|Fatal signal|Terminating app due to uncaught" "$logf"; then
            echo "mobilegate: $framework/$platform APP CRASHED before a verdict"
            grep -E "FATAL EXCEPTION|Fatal signal|Terminating app|Abort message" "$logf" | head -3
            rc=1; verdict=y; break; fi
        if ! kill -0 $runpid 2>/dev/null && grep -qiE "FAILURE|error:|✖|❌|Exception" "$runf" \
           && ! grep -qiE "Build Succeeded|BUILD SUCCESSFUL|Syncing files|Installing" "$runf"; then
            echo "mobilegate: $framework/$platform build failed"; tail -20 "$runf"; rc=1; verdict=y; break; fi
        sleep 2
    done
    # A silent expiry used to end the script with no output at all — say so,
    # and dump enough of both logs to diagnose without a rerun.
    if [ -z "$verdict" ]; then
        echo "mobilegate: $framework/$platform TIMEOUT — no verdict within 16min"
        echo "--- build tail ---"; tail -15 "$runf"
        echo "--- device-log tail ---"; tail -15 "$logf"
    fi
    kill $streampid $runpid 2>/dev/null || true
    return $rc
}

case "$platform" in
    ios)
        run "$ios_cmd" "xcrun simctl spawn booted log stream --level debug --predicate 'eventMessage CONTAINS \"MOBILEGATE\" OR eventMessage CONTAINS \"Terminating app\"'"
        ;;
    android)
        adb logcat -c 2>/dev/null || true
        run "$android_cmd" "adb logcat | grep --line-buffered -E 'MOBILEGATE|FATAL EXCEPTION|Fatal signal|Abort message'"
        ;;
    *) echo "unknown platform: $platform" >&2; exit 2 ;;
esac
