#!/usr/bin/env bash
# Assemble Kevy.xcframework from kevy-ffi static libs — the binary target
# the KevyKit SwiftPM package wraps.
#
# Usage: packaging/apple/build-xcframework.sh <outdir>
# Needs: Xcode CLT + rust targets aarch64-apple-{ios,ios-sim,darwin}.
set -euo pipefail

out="$1"
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

for t in aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin; do
  cargo build -p kevy-ffi --release --target "$t"
done

hdr="$root/crates/kevy-ffi/include"
rm -rf "$out/Kevy.xcframework"
mkdir -p "$out"
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libkevy_ffi.a -headers "$hdr" \
  -library target/aarch64-apple-ios-sim/release/libkevy_ffi.a -headers "$hdr" \
  -library target/aarch64-apple-darwin/release/libkevy_ffi.a -headers "$hdr" \
  -output "$out/Kevy.xcframework"

echo "built $out/Kevy.xcframework"
