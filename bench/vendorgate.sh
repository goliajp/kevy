#!/usr/bin/env bash
# vendorgate — vendored native artifacts must match the CURRENT ABI, gated.
#
# The mobile/runtime doors vendor prebuilt engine binaries (jniLibs .so,
# xcframeworks). Those go stale silently: the Rust source gains a symbol
# (kevy_get_shared, the ScalarGetSignal throw, …), every host test passes
# against a fresh build, and the door still ships the old binary — the fix
# never reaches a device. Caught live on 2026-07-16: the expo door's
# libkevy_jni.so predated the WRONGTYPE fix, and on-device GET-on-list
# returned a phantom miss while all host suites were green.
#
# This gate derives the expected symbol set FROM THE SOURCE (no hardcoded
# list to drift) and requires every vendored binary that exists to contain
# it. Gitignored artifacts that are absent are SKIPped (fresh clone);
# anything present must be current. Run it before mobilegate and before
# any release that ships vendored binaries:
#
#   bash bench/vendorgate.sh
#
# WHAT IT DOES NOT ANSWER, stated because the two look alike: this asks
# whether the artifact ON DISK is current. It does not ask whether the
# artifact reaches a user. flutter_kevy's four are gitignored, so they
# pass here and `dart pub publish` — which takes only what git tracks —
# omitted every one of them, producing a package that resolved and
# analysed clean with no engine in it. Publishability is
# scripts/mirror-flutter-package.sh's job: it reads the dry-run's own
# file list and refuses a package the engine is missing from.
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
fail=0
checked=0

# --- expected symbols, derived from the source of truth ------------------
# C ABI: every kevy_* function declared in the canonical header.
ffi_syms=$(grep -oE '\bkevy_[a-z0-9_]+\(' "$HERE/crates/kevy-ffi/include/kevy.h" \
    | sed 's/($//;s/(//' | sort -u)
# JNI: every exported Java_* entry point, plus the WRONGTYPE signal class
# the -2 branch throws (a stale .so lacks the string entirely).
jni_syms=$(grep -oE 'Java_[A-Za-z0-9_]+' "$HERE/crates/kevy-jni/src/lib.rs" | sort -u)
jni_syms="$jni_syms
jp/golia/kevy/ScalarGetSignal"

# require <file> <newline-separated symbols> — strings-based so it works on
# every slice (nm chokes on some rust-emitted objects; grep -a does not).
require() {
    local f="$1" syms="$2" missing=""
    checked=$((checked + 1))
    while IFS= read -r s; do
        [ -n "$s" ] || continue
        grep -aq "$s" "$f" || missing="$missing $s"
    done <<< "$syms"
    if [ -n "$missing" ]; then
        echo "STALE  $f"
        echo "       missing:$missing"
        fail=1
    else
        echo "ok     $f"
    fi
}

check_ffi() { [ -f "$1" ] && require "$1" "$ffi_syms" || echo "skip   $1 (not built)"; }
check_jni() { [ -f "$1" ] && require "$1" "$jni_syms" || echo "skip   $1 (not built)"; }

# headers vendored into xcframeworks must be byte-identical to canonical.
check_header() {
    local h="$1"
    [ -f "$h" ] || { echo "skip   $h (not built)"; return; }
    checked=$((checked + 1))
    if cmp -s "$h" "$HERE/crates/kevy-ffi/include/kevy.h"; then
        echo "ok     $h"
    else
        echo "STALE  $h (differs from crates/kevy-ffi/include/kevy.h)"
        fail=1
    fi
}

echo "== vendorgate: C-ABI binaries (need every kevy_* from kevy.h) =="
for so in \
    "$HERE"/bindings/expo/android/src/main/jniLibs/*/libkevy_ffi.so \
    "$HERE"/bindings/nitro/android/src/main/jniLibs/*/libkevy_ffi.so \
    "$HERE"/bindings/flutter/android/src/main/jniLibs/*/libkevy_ffi.so; do
    check_ffi "$so"
done
for a in \
    "$HERE"/bindings/apple/KevyKit/Artifacts/Kevy.xcframework/*/libkevy_ffi.a \
    "$HERE"/bindings/expo/ios/Kevy.xcframework/*/libkevy_ffi.a \
    "$HERE"/bindings/nitro/ios/KevyEngine.xcframework/*/libkevy_ffi.a; do
    check_ffi "$a"
done
for fw in "$HERE"/bindings/flutter/ios/kevy_ffi.xcframework/*/kevy_ffi.framework/kevy_ffi; do
    check_ffi "$fw"
done

echo "== vendorgate: JNI binaries (need every Java_* export + ScalarGetSignal) =="
for so in \
    "$HERE"/bindings/expo/android/src/main/jniLibs/*/libkevy_jni.so \
    "$HERE"/bindings/android/kevy/src/main/jniLibs/*/libkevy_jni.so; do
    check_jni "$so"
done

echo "== vendorgate: vendored headers (byte-identical to canonical kevy.h) =="
for h in \
    "$HERE"/bindings/apple/KevyKit/Artifacts/Kevy.xcframework/*/Headers/kevy.h \
    "$HERE"/bindings/expo/ios/Kevy.xcframework/*/Headers/kevy.h \
    "$HERE"/bindings/nitro/ios/KevyEngine.xcframework/*/Headers/kevy.h \
    "$HERE/bindings/nitro/cpp/kevy.h"; do
    check_header "$h"
done

echo
if [ "$fail" -ne 0 ]; then
    echo "vendorgate: FAIL — stale vendored artifacts; rebuild + re-vendor:"
    echo "  packaging/android/build-jnilibs.sh && packaging/android/build-ffi-jnilibs.sh"
    echo "  packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts"
    echo "  then each door's scripts/prepare-native.sh"
    exit 1
fi
echo "vendorgate: PASS ($checked artifacts current)"
