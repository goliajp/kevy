#!/usr/bin/env bash
# Copy the native artifacts expo-kevy vendors from their authoritative
# homes. Run before pod install / gradle build and before npm pack.
#
#   bash scripts/prepare-native.sh
#
# Prerequisites (from the repo root):
#   packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
#   packaging/android/build-jnilibs.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# iOS: the same stone KevyKit wraps.
rm -rf ios/Kevy.xcframework
cp -R ../apple/KevyKit/Artifacts/Kevy.xcframework ios/

# Android: the raw JNI class + the per-ABI engine cdylibs.
mkdir -p android/src/main/java/jp/golia/kevy
cp ../android/java/jp/golia/kevy/KevyNative.java android/src/main/java/jp/golia/kevy/
for pair in aarch64-linux-android:arm64-v8a x86_64-linux-android:x86_64; do
  triple=${pair%%:*}
  abi=${pair##*:}
  so=../../target/$triple/release/libkevy_jni.so
  if [ ! -f "$so" ]; then
    echo "missing $so — run packaging/android/build-jnilibs.sh first" >&2
    exit 1
  fi
  mkdir -p "android/src/main/jniLibs/$abi"
  cp "$so" "android/src/main/jniLibs/$abi/"
done

echo "expo-kevy native artifacts in place:"
find ios/Kevy.xcframework -name '*.a' | sed 's/^/  /'
find android/src/main/jniLibs -name '*.so' | sed 's/^/  /'
