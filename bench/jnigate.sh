#!/usr/bin/env bash
# jnigate — the raw Android/JVM door opens on the host, gated.
#
# The `bindings/android` door is hand-written JNI over kevy's C ABI plus a
# Kotlin shell; on a device it ships as an AAR. But the JNI surface itself
# is a pure-JVM object — no Android, no emulator — so it can be smoked on
# any JDK host: build libkevy_jni, compile the Java surface, and run
# Smoke.java, which exercises the command entry + scalar fast path, a
# pub/sub round trip, and reopen-durability, printing `smoke-jvm: ok`.
#
# This is the host-runnable complement to bench/mobilegate.sh (which drives
# the door's Kotlin/AAR form onto a real Android device — out of host scope
# without an emulator). CI needs only a JDK + cargo.
#
# Usage: bash bench/jnigate.sh
set -euo pipefail
cd "$(dirname "$0")/.."

command -v javac >/dev/null 2>&1 || { echo "jnigate: SKIP — no JDK (javac) on PATH"; exit 0; }

echo "jnigate: building libkevy_jni…"
cargo build -p kevy-jni

# The native lib lands in target/debug as libkevy_jni.{dylib,so}; that dir
# is java.library.path for System.loadLibrary("kevy_jni").
libdir="target/debug"
[ -f "$libdir/libkevy_jni.dylib" ] || [ -f "$libdir/libkevy_jni.so" ] \
    || { echo "jnigate: FAIL — libkevy_jni not found under $libdir"; exit 1; }

DIR=$(mktemp -d /tmp/kevy-jnigate-XXXXXX)
trap 'rm -rf "$DIR"' EXIT

javac -d "$DIR/classes" bindings/android/java/jp/golia/kevy/*.java

out="$(java -Djava.library.path="$libdir" -cp "$DIR/classes" \
        jp.golia.kevy.Smoke "$DIR/data" 2>&1)" || {
    echo "jnigate: FAIL — Smoke exited non-zero"; echo "$out"; exit 1
}
echo "$out"
case "$out" in
    *"smoke-jvm: ok"*) echo "jnigate: PASS — raw JNI door opens on the JVM" ;;
    *) echo "jnigate: FAIL — no 'smoke-jvm: ok' in output"; exit 1 ;;
esac
