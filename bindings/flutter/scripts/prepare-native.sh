#!/usr/bin/env bash
# Vendor the native artifacts flutter_kevy carries, from their
# authoritative homes. Run before `flutter run` / `flutter build` and
# before publishing.
#
#   bash scripts/prepare-native.sh
#
# Prerequisites (from the repo root):
#   packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
#   packaging/android/build-ffi-jnilibs.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# iOS: a DYNAMIC-framework xcframework the app embeds + code-signs, so
# dart:ffi DynamicLibrary.open resolves it via @rpath (the static
# xcframework KevyKit/expo use is not Swift-import-linkable from a plain
# CocoaPods pod). Built fresh from kevy-ffi so it carries current perf.
rm -rf ios/kevy_ffi.xcframework
../../packaging/apple/build-dynamic-xcframework.sh ios/.dyn-fw
mv ios/.dyn-fw/kevy_ffi.xcframework ios/
rm -rf ios/.dyn-fw

# Android: the per-ABI kevy-ffi cdylibs, as jniLibs.
for pair in aarch64-linux-android:arm64-v8a x86_64-linux-android:x86_64; do
  triple=${pair%%:*}
  abi=${pair##*:}
  so=../../target/$triple/release/libkevy_ffi.so
  if [ ! -f "$so" ]; then
    echo "missing $so — run packaging/android/build-ffi-jnilibs.sh first" >&2
    exit 1
  fi
  mkdir -p "android/src/main/jniLibs/$abi"
  cp "$so" "android/src/main/jniLibs/$abi/"
done

echo "flutter_kevy native artifacts in place:"
find ios/kevy_ffi.xcframework -name 'kevy_ffi' -type f | sed 's/^/  /'
find android/src/main/jniLibs -name '*.so' | sed 's/^/  /'
