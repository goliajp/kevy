#!/usr/bin/env bash
# Vendor the native artifacts react-native-kevy-nitro carries, from their
# authoritative homes. Run before pod install / gradle build.
#
#   bash scripts/prepare-native.sh
#
# Prerequisites (from the repo root):
#   packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
#   packaging/android/build-ffi-jnilibs.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# iOS: the same stone KevyKit wraps, renamed KevyEngine — and with the
# `module Kevy` modulemap STRIPPED. The C++ HybridObject includes the local
# cpp/kevy.h and links the .a for symbols only; keeping the modulemap would
# redefine module 'Kevy' against the Expo door's Kevy.xcframework in the
# dual-door example app (clang: "redefinition of module 'Kevy'"). This
# recipe used to live only in the door's wiring commit message — which is
# exactly how the 5.0.0 regeneration re-tripped it.
rm -rf ios/KevyEngine.xcframework
cp -R ../apple/KevyKit/Artifacts/Kevy.xcframework ios/KevyEngine.xcframework
find ios/KevyEngine.xcframework -name module.modulemap -delete

# Android: the per-ABI kevy-ffi cdylibs, as jniLibs (dlopen'd by the C++
# door — same artifact the Flutter plugin vendors).
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

echo "react-native-kevy-nitro native artifacts in place:"
find ios/KevyEngine.xcframework -name '*.a' | sed 's/^/  /'
find ios/KevyEngine.xcframework -name module.modulemap | sed 's/^/  STALE MODULEMAP: /'
find android/src/main/jniLibs -name '*.so' | sed 's/^/  /'
