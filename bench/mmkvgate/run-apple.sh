#!/usr/bin/env bash
# Run mmkvgate (Apple) on one simulator or device and print its table.
#
#   bash bench/mmkvgate/run-apple.sh <udid>
#
# A device needs the GOLIA signing identity in the keychain. The tests
# build in Release; the kevy engine comes from Kevy.xcframework, so rebuild
# it first when the engine changed:
#   packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
set -euo pipefail
. "$(dirname "$0")/../bench-lock.sh"   # hold the machine's bench lock for the whole run

udid="${1:?usage: run-apple.sh <simulator-or-device-udid>}"
here="$(cd "$(dirname "$0")/apple" && pwd)"
log="$(mktemp -t mmkvgate)"
cd "$here"
xcodegen generate --quiet
if ! xcodebuild test -project mmkvgate.xcodeproj -scheme mmkvgate \
    -destination "id=$udid" -derivedDataPath .build/dd-"$udid" \
    -allowProvisioningUpdates >"$log" 2>&1; then
  grep -E "error:|failed|Fatal" "$log" | head -20
  echo "mmkvgate: xcodebuild failed, full log in $log" >&2
  exit 1
fi
rows=$(grep -E '^MMKVGATE ' "$log" | sort -u)
# six tests produce fourteen cells; fewer means an axis silently did not run
n=$(printf '%s\n' "$rows" | grep -c . || true)
if [ "$n" -ne 14 ]; then
  echo "mmkvgate: expected 14 cells, got $n (log: $log)" >&2
  exit 1
fi
printf '%-14s %6s %12s %12s %10s\n' axis bytes kevy_ns mmkv_ns kevy/mmkv
printf '%s\n' "$rows" | sed -E 's/kevy_ns=|mmkv_ns=|kevy\/mmkv=//g' |
  awk '{printf "%-14s %6s %12s %12s %10s\n", $2, $3, $4, $5, $6}'
