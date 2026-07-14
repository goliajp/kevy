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

# iOS: the same stone KevyKit and expo-kevy wrap.
rm -rf ios/Kevy.xcframework
cp -R ../apple/KevyKit/Artifacts/Kevy.xcframework ios/

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
find ios/Kevy.xcframework -name '*.a' | sed 's/^/  /'
find android/src/main/jniLibs -name '*.so' | sed 's/^/  /'
