#!/usr/bin/env bash
# Build kevy-ffi as a DYNAMIC-framework xcframework for the flutter door.
#
# The flutter door loads the engine through dart:ffi
# DynamicLibrary.open(), the same way the Android door loads its dynamic
# .so — a dynamic framework sidesteps the CocoaPods static-xcframework
# module/link integration the static build fought (Swift `import` module
# not registered, `-lkevy_ffi` slice not linked). KevyKit / expo keep the
# STATIC xcframework (their Swift shells call the symbols directly); this
# is the flutter-only dynamic variant.
#
#   packaging/apple/build-dynamic-xcframework.sh <out-dir>
set -euo pipefail
# Resolve <out-dir> against the CALLER's cwd BEFORE cd'ing to the repo
# root, or a relative out-dir silently lands under the repo root (bit the
# flutter prepare-native.sh, which passes ios/.dyn-fw relative to its door).
OUT=${1:?usage: build-dynamic-xcframework.sh <out-dir>}
OUT=$(mkdir -p "$OUT" && cd "$OUT" && pwd)
cd "$(dirname "$0")/../.."
HDR="$PWD/crates/kevy-ffi/include"
FW=kevy_ffi

build_slice() { # slice platform triple...
  local slice=$1 platform=$2; shift 2
  local triples=("$@") dylibs=()
  for t in "${triples[@]}"; do
    cargo build -q -p kevy-ffi --target "$t" --release
    dylibs+=("target/$t/release/libkevy_ffi.dylib")
  done
  local fwdir="$OUT/$slice/$FW.framework"
  rm -rf "$fwdir"; mkdir -p "$fwdir/Headers" "$fwdir/Modules"
  # A simulator slice must cover every arch the app builds for (Apple
  # Silicon arm64 + Intel x86_64) or CocoaPods rejects it: "Unable to find
  # matching slice ... for architectures (arm64 x86_64)". lipo the arches
  # into one fat binary.
  lipo -create "${dylibs[@]}" -output "$fwdir/$FW"
  # @rpath so the app can embed + load it relative to itself.
  install_name_tool -id "@rpath/$FW.framework/$FW" "$fwdir/$FW"
  cp "$HDR/kevy.h" "$fwdir/Headers/"
  cat > "$fwdir/Modules/module.modulemap" <<EOF
framework module $FW {
    header "kevy.h"
    export *
}
EOF
  cat > "$fwdir/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>$FW</string>
  <key>CFBundleIdentifier</key><string>jp.golia.kevyffi</string>
  <key>CFBundleName</key><string>$FW</string>
  <key>CFBundlePackageType</key><string>FMWK</string>
  <key>CFBundleVersion</key><string>4.0.0</string>
  <key>CFBundleShortVersionString</key><string>4.0.0</string>
  <key>MinimumOSVersion</key><string>15.0</string>
  <key>CFBundleSupportedPlatforms</key><array><string>$platform</string></array>
</dict></plist>
EOF
  codesign --force --sign - "$fwdir"
}

build_slice device iPhoneOS       aarch64-apple-ios
build_slice sim    iPhoneSimulator aarch64-apple-ios-sim x86_64-apple-ios

rm -rf "$OUT/$FW.xcframework"
xcodebuild -create-xcframework \
  -framework "$OUT/device/$FW.framework" \
  -framework "$OUT/sim/$FW.framework" \
  -output "$OUT/$FW.xcframework"
echo "dynamic xcframework: $OUT/$FW.xcframework"
