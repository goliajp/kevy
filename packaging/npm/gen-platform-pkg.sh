#!/usr/bin/env bash
# Generate one @goliapkg/kevy-bin-<os>-<arch> platform package from built
# kevy / kevy-cli binaries. The platform package carries the raw binaries
# plus the os/cpu fields npm uses to install only the matching one.
#
# Usage:
#   packaging/npm/gen-platform-pkg.sh <os> <cpu> <kevy-bin> <kevy-cli-bin> <outdir>
#   e.g. gen-platform-pkg.sh darwin arm64 target/release/kevy \
#          target/release/kevy-cli /tmp/npm-stage
#
# CI calls this once per release target, then `npm publish` each result and
# the meta package. Locally it feeds the packaging smoke test.
set -euo pipefail

os="$1" cpu="$2" kevy_bin="$3" cli_bin="$4" outdir="$5"
version="$(grep -m1 '"version"' "$(dirname "$0")/kevy-bin/package.json" | sed 's/.*: "\(.*\)".*/\1/')"

pkg="$outdir/kevy-bin-$os-$cpu"
mkdir -p "$pkg"
cp "$kevy_bin" "$pkg/kevy"
cp "$cli_bin" "$pkg/kevy-cli"
chmod +x "$pkg/kevy" "$pkg/kevy-cli"

cat > "$pkg/package.json" <<EOF
{
  "name": "@goliapkg/kevy-bin-$os-$cpu",
  "version": "$version",
  "description": "kevy binaries for $os-$cpu. Install @goliapkg/kevy-bin instead of this directly.",
  "license": "(Apache-2.0 OR MIT)",
  "repository": { "type": "git", "url": "git+https://github.com/goliajp/kevy.git" },
  "os": ["$os"],
  "cpu": ["$cpu"],
  "files": ["kevy", "kevy-cli"]
}
EOF

echo "staged $pkg (version $version)"
