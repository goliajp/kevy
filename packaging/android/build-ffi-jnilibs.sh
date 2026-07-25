#!/usr/bin/env bash
# Build libkevy_ffi.so for the Android ABIs the Flutter plugin vendors.
# Flutter's dart:ffi loads the engine cdylib directly (no JNI), so this
# builds kevy-ffi (not kevy-jni — that's the RN/Kotlin door). Twin of
# packaging/android/build-jnilibs.sh; artifacts land under
# target/<triple>/release.
#
#   packaging/android/build-ffi-jnilibs.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

NDK="${ANDROID_NDK_HOME:-$(ls -d "$HOME"/Library/Android/sdk/ndk/* 2>/dev/null | sort -V | tail -1)}"
[ -n "$NDK" ] && [ -d "$NDK" ] || { echo "NDK not found — set ANDROID_NDK_HOME" >&2; exit 1; }

HOST="$(uname -s | tr '[:upper:]' '[:lower:]')-x86_64"
BIN="$NDK/toolchains/llvm/prebuilt/$HOST/bin"
API=24

for triple in aarch64-linux-android x86_64-linux-android; do
  upper="$(echo "$triple" | tr '[:lower:]-' '[:upper:]_')"
  export "CARGO_TARGET_${upper}_LINKER=$BIN/${triple}${API}-clang"
  # Set an explicit SONAME. Flutter's dart:ffi dlopen()s by absolute path so
  # it never needed one, but a consumer that *links* against this .so (the
  # RN/Nitro C++ door does) records the DT_NEEDED from the SONAME — without
  # it the linker bakes in the build-time path and dlopen fails at runtime.
  RUSTFLAGS="-C link-arg=-Wl,-soname,libkevy_ffi.so" \
    cargo build -p kevy-ffi --release --target "$triple"
done

echo "kevy-ffi jniLibs ready:"
ls -l target/{aarch64,x86_64}-linux-android/release/libkevy_ffi.so
