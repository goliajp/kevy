#!/usr/bin/env bash
# The deb packaging smoke: build the .deb, extract it into a scratch root,
# and prove both installed binaries run — server answers PING, a value
# round-trips through SET/GET. This is what CI runs before shipping a .deb.
#
# We extract with `dpkg-deb --extract` rather than `dpkg -i` so the smoke
# needs no root and touches no system state: the package is two binaries at
# usr/bin/, and extraction proves the layout `dpkg -i` would install.
#
# Usage:
#   packaging/deb/smoke.sh <version> <deb-arch> <kevy-bin> <kevy-cli-bin> <scratch-dir>
#   e.g. smoke.sh 4.0.0 arm64 target/release/kevy target/release/kevy-cli /tmp/kevy-deb-smoke
#
# This is a Linux packaging smoke (dpkg-deb). On a host without dpkg-deb it
# skips loudly with exit 0 — CI runs it on ubuntu where the tools exist.
set -euo pipefail

v="$1" arch="$2" kevy_bin="$3" cli_bin="$4" scratch="$5"
here="$(cd "$(dirname "$0")" && pwd)"

if ! command -v dpkg-deb >/dev/null 2>&1; then
    echo "deb-smoke: SKIP — dpkg-deb not found (Linux packaging smoke; run on a Debian/Ubuntu host)"
    exit 0
fi

rm -rf "$scratch"
mkdir -p "$scratch/dist" "$scratch/root"

# 1. Build the .deb from the given binaries.
"$here/build-deb.sh" "$v" "$arch" "$kevy_bin" "$cli_bin" "$scratch/dist"
deb="$scratch/dist/kevy_${v}_${arch}.deb"
[ -f "$deb" ] || { echo "deb-smoke: FAIL — build-deb.sh produced no $deb"; exit 1; }

# 2. Extract it as `dpkg -i` would lay it out, no root required.
dpkg-deb --extract "$deb" "$scratch/root"
kevy="$scratch/root/usr/bin/kevy"
cli="$scratch/root/usr/bin/kevy-cli"
[ -x "$kevy" ] || { echo "deb-smoke: FAIL — no executable kevy at $kevy"; exit 1; }
[ -x "$cli" ]  || { echo "deb-smoke: FAIL — no executable kevy-cli at $cli"; exit 1; }

# 3. Run the extracted binaries: boot the server, PING, SET/GET, shut down.
port="${KEVY_DEB_SMOKE_PORT:-7531}"
data="$scratch/data"
srvpid=""
cleanup() { [ -n "$srvpid" ] && kill "$srvpid" 2>/dev/null || true; }
trap cleanup EXIT

env KEVY_BIND=127.0.0.1 "$kevy" --port "$port" --dir "$data" > "$scratch/server.log" 2>&1 &
srvpid=$!

for _ in $(seq 100); do
    "$cli" -p "$port" PING >/dev/null 2>&1 && break
    sleep 0.1
done
pong="$("$cli" -p "$port" PING 2>/dev/null || true)"
case "$pong" in
    PONG|+PONG) ;;
    *) echo "deb-smoke: FAIL — server never answered PING (got: '$pong')"; tail -5 "$scratch/server.log"; exit 1 ;;
esac

"$cli" -p "$port" SET deb:k hello >/dev/null
# kevy-cli prints bulk-string replies quoted ("hello"); match the value, not
# the exact framing.
got="$("$cli" -p "$port" GET deb:k 2>/dev/null || true)"
case "$got" in *hello*) ;; *) echo "deb-smoke: FAIL — GET returned '$got', want hello"; exit 1 ;; esac

echo "kevy      -> $("$kevy" --version)"
echo "kevy-cli  -> $("$cli" --version)"
echo "deb-smoke: ok"
