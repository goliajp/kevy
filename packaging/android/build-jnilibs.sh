#!/usr/bin/env bash
# Build libkevy_jni.so for the Android ABIs the mobile packages vendor
# (the AAR and expo-kevy). Mirrors packaging/apple/build-xcframework.sh:
# run it from the repo root, artifacts land under target/<triple>/release.
#
#   packaging/android/build-jnilibs.sh
#
# Needs an NDK (ANDROID_NDK_HOME, or the newest one under the default
# macOS SDK path) and the rustup targets aarch64/x86_64-linux-android.
set -euo pipefail
cd "$(dirname "$0")/../.."

NDK="${ANDROID_NDK_HOME:-$(ls -d "$HOME"/Library/Android/sdk/ndk/* 2>/dev/null | sort -V | tail -1)}"
[ -n "$NDK" ] && [ -d "$NDK" ] || { echo "NDK not found — set ANDROID_NDK_HOME" >&2; exit 1; }

HOST="$(uname -s | tr '[:upper:]' '[:lower:]')-x86_64" # darwin-x86_64 even on arm64 macs
BIN="$NDK/toolchains/llvm/prebuilt/$HOST/bin"
API=24

for triple in aarch64-linux-android x86_64-linux-android; do
  upper="$(echo "$triple" | tr '[:lower:]-' '[:upper:]_')"
  export "CARGO_TARGET_${upper}_LINKER=$BIN/${triple}${API}-clang"
  cargo build -p kevy-jni --release --target "$triple"
done

echo "jniLibs ready:"
ls -l target/{aarch64,x86_64}-linux-android/release/libkevy_jni.so
