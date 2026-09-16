#!/usr/bin/env bash
# Generate one @goliapkg/kevy-bin-<os>-<cpu> platform package from built
# kevy / kevy-cli binaries.
#
# The esbuild pattern: `@goliapkg/kevy-bin` declares one optionalDependency
# per (os, cpu) pair and ships only a launcher; npm installs exactly the
# matching platform package and `bin/resolve.js` execs the real binary out
# of it. The binary IS the package — no postinstall, no network fetch. So
# the launcher is unusable on its own: without its platform package on the
# registry, `kevy` dies with "kevy-bin-<platform> is not installed".
#
#   packaging/npm/gen-platform-pkg.sh <os> <cpu> <kevy> <kevy-cli> <outdir>
#
# The version is the launcher's, read from kevy-bin/package.json, so the
# pair cannot be generated at two different versions. The (os, cpu) pair
# must be one of bin/resolve.js's PLATFORMS keys, or a matching install
# still finds nothing.
#
# Called by smoke.sh, which .github/workflows/npm-platform.yml runs on each
# release target against the binaries attached to the GitHub release — so
# what npm serves is byte-identical to the tarball users download by hand.
set -euo pipefail

os="$1" cpu="$2" kevy_bin="$3" cli_bin="$4" outdir="$5"
here="$(cd "$(dirname "$0")" && pwd)"
version="$(node -p "require('$here/kevy-bin/package.json').version")"

grep -q "\"$os $cpu\"" "$here/kevy-bin/bin/resolve.js" \
  || { echo "gen-platform-pkg: '$os $cpu' is not a platform resolve.js knows" >&2; exit 1; }

pkg="$outdir/kevy-bin-$os-$cpu"
rm -rf "$pkg"
mkdir -p "$pkg"
# resolve.js joins the binary name onto the package root, not bin/.
cp "$kevy_bin" "$pkg/kevy"
cp "$cli_bin" "$pkg/kevy-cli"
chmod +x "$pkg/kevy" "$pkg/kevy-cli"
cp "$here/../../LICENSE-MIT" "$here/../../LICENSE-APACHE" "$pkg/"

cat > "$pkg/package.json" <<EOF
{
  "name": "@goliapkg/kevy-bin-$os-$cpu",
  "version": "$version",
  "description": "The kevy server and CLI binaries for $os $cpu. Installed automatically by @goliapkg/kevy-bin; not meant to be depended on directly.",
  "license": "(Apache-2.0 OR MIT)",
  "repository": { "type": "git", "url": "git+https://github.com/goliajp/kevy.git" },
  "homepage": "https://kevy.golia.jp",
  "os": ["$os"],
  "cpu": ["$cpu"],
  "files": ["kevy", "kevy-cli", "LICENSE-MIT", "LICENSE-APACHE"],
  "preferUnplugged": true
}
EOF

cat > "$pkg/README.md" <<EOF
# @goliapkg/kevy-bin-$os-$cpu

Prebuilt \`kevy\` and \`kevy-cli\` binaries for **$os $cpu**, from the
[v$version release](https://github.com/goliajp/kevy/releases/tag/v$version).

You do not install this package directly. \`@goliapkg/kevy-bin\` declares it
as an optional dependency; your package manager picks the one matching your
platform and the launcher execs the binary out of it.

\`\`\`sh
npm install -g @goliapkg/kevy-bin
kevy --port 6379
\`\`\`

Building from source instead: \`cargo install kevy kevy-cli\`.
EOF

echo "staged $pkg (version $version)"
