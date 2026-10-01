#!/usr/bin/env bash
# Run mmkvgate (Android) on one device and print its table.
#
#   SMIX_WORKSPACE=<dir> bash bench/mmkvgate/run-android.sh <serial> <runner-port>
#
# The device is driven through smix: it installs the app, launches it and
# reads the finished table off the screen. The runner must be up on that
# device and port (`smix runner up <serial> --platform android
# --runner-port <port>`).
set -euo pipefail

dev="${1:?usage: run-android.sh <serial> <runner-port>}"
port="${2:?usage: run-android.sh <serial> <runner-port>}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
here="$root/bench/mmkvgate/android"
cd "$root"
bash packaging/android/build-jnilibs.sh >/dev/null
mkdir -p "$here/gate/build/jni/arm64-v8a"
cp target/aarch64-linux-android/release/libkevy_jni.so "$here/gate/build/jni/arm64-v8a/"
( cd "$here" && ./gradlew -q :gate:assembleRelease )

export SMIX_RUNNER_PORT="$port"
smix sim install "$dev" "$here/gate/build/outputs/apk/release/gate-release.apk" >/dev/null
smix sim terminate "$dev" jp.golia.kevy.mmkvgate >/dev/null 2>&1 || true
smix sim launch "$dev" jp.golia.kevy.mmkvgate >/dev/null
smix wait-for "id:mmkvgate_done" --timeout 900 --device "$dev" >/dev/null
rows=$(smix tree --json --device "$dev" |
  grep -oE 'MMKVGATE [a-z_0-9]+ [0-9]+ kevy_ns=[0-9.]+ mmkv_ns=[0-9.]+ kevy/mmkv=[0-9.]+' | sort -u)
# six axes groups produce fourteen cells; fewer means one silently did not run
n=$(printf '%s\n' "$rows" | grep -c . || true)
if [ "$n" -ne 14 ]; then
  echo "mmkvgate: expected 14 cells, got $n" >&2
  exit 1
fi
printf '%-14s %6s %12s %12s %10s\n' axis bytes kevy_ns mmkv_ns kevy/mmkv
printf '%s\n' "$rows" | sed -E 's/kevy_ns=|mmkv_ns=|kevy\/mmkv=//g' |
  awk '{printf "%-14s %6s %12s %12s %10s\n", $2, $3, $4, $5, $6}'
