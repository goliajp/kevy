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

# A device run against a stale vendored engine binary tests yesterday's
# code with today's green suites (caught live 2026-07-16) — gate it first,
# it is seconds against a 10-minute build.
bash "$HERE/bench/vendorgate.sh" >/dev/null 2>&1 || {
    echo "mobilegate: vendored natives are STALE — run bench/vendorgate.sh for the list" >&2
    exit 1
}

if ! command -v npx >/dev/null 2>&1; then
    nvm_bin=$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -1)
    [ -n "$nvm_bin" ] && export PATH="$nvm_bin:$PATH"
fi
command -v flutter >/dev/null 2>&1 || export PATH="/opt/homebrew/bin:$PATH"

ios_sim_id() {
    xcrun simctl list devices booted -j | grep -o '"udid" : "[^"]*"' | head -1 | sed 's/.*: "//;s/"//'
}
# The target device, honouring ANDROID_SERIAL (adb's own variable) when
# it is set. Without it this takes whatever `adb devices` lists first,
# which on a machine with a phone plugged in AND an emulator running is a
# coin toss — and the losing side of that toss installs a gate build onto
# somebody's actual phone.
android_dev_id() {
    if [ -n "${ANDROID_SERIAL:-}" ]; then
        printf '%s' "$ANDROID_SERIAL"
        return
    fi
    adb devices | grep -w device | grep -v List | head -1 | cut -f1
}

# The same device, named the way `expo run:android --device` wants it.
# Flutter's `-d` takes the adb serial; expo takes the DEVICE NAME, and for
# an emulator that is its AVD name — passing the serial there fails with
# "Could not find device with name: emulator-5554".
android_expo_device() {
    id=$(android_dev_id)
    case "$id" in
        emulator-*) adb -s "$id" emu avd name 2>/dev/null | head -1 | tr -d '\r' ;;
        *) printf '%s' "$id" ;;
    esac
}

case "$framework" in
    expo)
        appdir="$HERE/bindings/expo/example"
        [ -d "$appdir/node_modules" ] || ( cd "$appdir" && npm install --no-audit --no-fund )
        # iOS gates on a Release build: the JS bundle is EMBEDDED, so no Metro
        # is involved — a dev build fetches its bundle over localhost and a
        # stray Metro from another project on the default port feeds it a
        # FOREIGN bundle (observed: crashed on that app's missing native
        # modules, and the dev client ignored RCT_jsLocation overrides).
        # arm64-only for the same reason as the barern recipe below: the
        # vendored sim slice is arm64, and a fat Release build finds no
        # matching slice (CocoaPods then copies NOTHING and the Swift shell
        # fails with "no such module 'Kevy'").
        # `pod install` runs EVERY time, not just when the workspace is
        # missing. CocoaPods copies the generated nitro headers into the
        # pod's public header dir at install time, so a header that appears
        # after the last install is simply absent from the build — which
        # surfaces as `'KevyOpenStats.hpp' file not found` deep inside a
        # Swift module error, not as anything resembling "your pods are
        # stale" (observed 2026-07-22). Same reasoning as the vendorgate
        # check above: a gate that builds against a stale input is testing
        # yesterday's code and reporting on today's.
        ios_cmd='sim=$(ios_sim_id); \
          ( cd ios && pod install ); \
          xcodebuild -workspace ios/HelloWorld.xcworkspace -scheme HelloWorld \
            -configuration Release -sdk iphonesimulator \
            -destination "id=$sim" ONLY_ACTIVE_ARCH=YES ARCHS=arm64 EXCLUDED_ARCHS=x86_64 \
            CODE_SIGNING_ALLOWED=NO build && \
          app=$(xcodebuild -workspace ios/HelloWorld.xcworkspace -scheme HelloWorld \
            -configuration Release -sdk iphonesimulator -showBuildSettings 2>/dev/null \
            | awk -F" = " "/ BUILT_PRODUCTS_DIR/{print \$2; exit}")/HelloWorld.app && \
          xcrun simctl install "$sim" "$app" && \
          xcrun simctl launch "$sim" com.anonymous.expo-template-blank-typescript'
        # Android keeps the dev build (Metro) but on a dedicated port for the
        # same shared-box reason.
        android_cmd="npx expo run:android --port 8087 --device $(android_expo_device)"
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
          ( cd ios && pod install ); \
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
