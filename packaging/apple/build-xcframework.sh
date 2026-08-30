#!/usr/bin/env bash
# Assemble Kevy.xcframework from kevy-ffi static libs — the binary target
# the KevyKit SwiftPM package wraps.
#
# Usage: packaging/apple/build-xcframework.sh <outdir>
# Needs: Xcode CLT + rust targets aarch64-apple-{ios,ios-sim,darwin}.
set -euo pipefail

# Keep the builder's home out of the artifact. These libraries are shipped
# inside public npm and pub.dev packages, and on a machine with the
# `rust-src` component rustc resolves std's panic locations to the local
# toolchain source instead of the `/rustc/<hash>/` form official builds
# carry — putting hundreds of copies of /Users/<name>/.rustup/... in a file
# that goes to a registry. Who built it is not something it should say.
REMAP="--remap-path-prefix=$HOME=~"

out="$1"
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

for t in aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin; do
  RUSTFLAGS="${RUSTFLAGS:-} $REMAP" \
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
