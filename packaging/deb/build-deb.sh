#!/usr/bin/env bash
# Build one kevy .deb (server + cli) from prebuilt binaries. Hand-rolled
# layout + dpkg-deb, no packaging framework: the package is two binaries,
# a control file, and nothing clever.
#
# Usage:
#   packaging/deb/build-deb.sh <version> <deb-arch> <kevy-bin> <kevy-cli-bin> <outdir>
#   e.g. build-deb.sh 4.0.0 arm64 target/release/kevy target/release/kevy-cli dist/
#
# deb-arch is dpkg's name: amd64 | arm64.
set -euo pipefail

v="$1" arch="$2" kevy_bin="$3" cli_bin="$4" outdir="$5"

root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT

mkdir -p "$root/usr/bin" "$root/DEBIAN" \
         "$root/usr/share/doc/kevy" "$root/lib/systemd/system"
install -m 0755 "$kevy_bin" "$root/usr/bin/kevy"
install -m 0755 "$cli_bin" "$root/usr/bin/kevy-cli"

cat > "$root/DEBIAN/control" <<EOF
Package: kevy
Version: $v
Section: database
Priority: optional
Architecture: $arch
Maintainer: GOLIA K.K. <admin@golia.jp>
Homepage: https://kevy.golia.jp
Description: Redis-compatible data layer for AI systems
 kevy server and kevy-cli. Redis-compatible wire protocol, faster on
 every operation, with vector search, full-text, secondary indexes,
 materialised views and a change feed in the engine.
EOF

# A unit users can enable; the package does not enable or start it.
cat > "$root/lib/systemd/system/kevy.service" <<'EOF'
[Unit]
Description=kevy server
After=network.target

[Service]
ExecStart=/usr/bin/kevy --dir /var/lib/kevy
StateDirectory=kevy
DynamicUser=yes
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF

cp LICENSE-APACHE LICENSE-MIT "$root/usr/share/doc/kevy/" 2>/dev/null || true

mkdir -p "$outdir"
dpkg-deb --build --root-owner-group "$root" "$outdir/kevy_${v}_${arch}.deb"
echo "built $outdir/kevy_${v}_${arch}.deb"
