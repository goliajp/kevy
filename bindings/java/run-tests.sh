#!/usr/bin/env bash
# run-tests.sh — build libkevy_jni + the kevy server, compile the Java client
# and its JUnit 5 conformance suite, and run the suite against BOTH backends
# (embedded mem://+file:// over the JNI door, and a real remote kevy server).
#
# Maven/Gradle are not needed: we compile with javac and drive JUnit 5 through
# the junit-platform-console-standalone jar (fetched into lib/ once), exactly
# like bench/jnigate.sh drives the raw JNI door. CI needs a JDK + cargo.
#
# Usage: bash bindings/java/run-tests.sh [junit-console-args…]
set -euo pipefail
cd "$(dirname "$0")"
HERE="$(pwd)"

# The main checkout (worktrees keep no target/ of their own).
MAIN_ROOT="$(cd "$(git rev-parse --git-common-dir)/.." && pwd)"

command -v javac >/dev/null 2>&1 || { echo "run-tests: SKIP — no JDK (javac)"; exit 0; }

JUNIT_JAR="$HERE/lib/junit-platform-console-standalone-1.10.2.jar"
if [ ! -f "$JUNIT_JAR" ]; then
  echo "run-tests: fetching JUnit 5 console standalone…"
  mkdir -p "$HERE/lib"
  curl -sSL -o "$JUNIT_JAR" \
    https://repo1.maven.org/maven2/org/junit/platform/junit-platform-console-standalone/1.10.2/junit-platform-console-standalone-1.10.2.jar
fi

echo "run-tests: building libkevy_jni + kevy server…"
( cd "$MAIN_ROOT" && cargo build -p kevy-jni && cargo build --release -p kevy )

JNI_DIR="$MAIN_ROOT/target/debug"
SERVER_BIN="$MAIN_ROOT/target/release/kevy"
[ -f "$JNI_DIR/libkevy_jni.dylib" ] || [ -f "$JNI_DIR/libkevy_jni.so" ] \
  || { echo "run-tests: FAIL — libkevy_jni not found under $JNI_DIR"; exit 1; }
[ -x "$SERVER_BIN" ] || { echo "run-tests: FAIL — kevy server not found at $SERVER_BIN"; exit 1; }

OUT="$HERE/target"
rm -rf "$OUT"; mkdir -p "$OUT/classes" "$OUT/test-classes"

echo "run-tests: compiling main…"
find src/main/java -name '*.java' > "$OUT/main.txt"
javac -d "$OUT/classes" @"$OUT/main.txt"

echo "run-tests: compiling tests…"
find src/test/java -name '*.java' > "$OUT/test.txt"
javac -cp "$OUT/classes:$JUNIT_JAR" -d "$OUT/test-classes" @"$OUT/test.txt"

echo "run-tests: running JUnit 5…"
export KEVY_SERVER_BIN="$SERVER_BIN"
java -Djava.library.path="$JNI_DIR" \
  -jar "$JUNIT_JAR" execute \
  -cp "$OUT/classes:$OUT/test-classes" \
  --scan-classpath \
  --details=tree \
  --disable-banner \
  "$@"
