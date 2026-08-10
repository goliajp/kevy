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

# Android: the raw JNI class + the per-ABI engine cdylibs. Both engine
# libs are vendored: the JNI door loads libkevy_jni; libkevy_ffi rides
# along for the dual-door example, where the Nitro AAR packages the same
# file — AGP's mergeNativeLibs dedupes byte-identical duplicates but
# ERRORS on differing ones, so a stale copy here breaks the example app
# build the moment the Nitro side re-vendors (caught live by mobilegate
# expo/android after the 5.0.0 re-vendor; this script used to copy only
# the JNI lib, leaving the ffi copy to drift).
mkdir -p android/src/main/java/jp/golia/kevy
cp ../android/java/jp/golia/kevy/KevyNative.java android/src/main/java/jp/golia/kevy/
for pair in aarch64-linux-android:arm64-v8a x86_64-linux-android:x86_64; do
  triple=${pair%%:*}
  abi=${pair##*:}
  mkdir -p "android/src/main/jniLibs/$abi"
  for lib in libkevy_jni.so libkevy_ffi.so; do
    so=../../target/$triple/release/$lib
    if [ ! -f "$so" ]; then
      echo "missing $so — run packaging/android/build-jnilibs.sh + build-ffi-jnilibs.sh first" >&2
      exit 1
    fi
    cp "$so" "android/src/main/jniLibs/$abi/"
  done
done

echo "expo-kevy native artifacts in place:"
find ios/Kevy.xcframework -name '*.a' | sed 's/^/  /'
find android/src/main/jniLibs -name '*.so' | sed 's/^/  /'
