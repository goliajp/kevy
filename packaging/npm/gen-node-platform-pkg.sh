#!/usr/bin/env bash
# Generate one @goliapkg/kevy-node-<os>-<arch> platform package from built
# native artifacts: the N-API addon (kevy.node, for Node) and the raw
# cdylib (for bun:ffi). The main @goliapkg/kevy-node package lists these
# as optionalDependencies — npm/bun install only the matching one, and
# the loaders resolve into it (see bindings/node/node.js / bun.js).
#
# Usage:
#   packaging/npm/gen-node-platform-pkg.sh <os> <cpu> <libkevy_napi> <libkevy_ffi> <outdir>
#   e.g. gen-node-platform-pkg.sh darwin arm64 \
#          target/release/libkevy_napi.dylib target/release/libkevy_ffi.dylib \
#          /tmp/npm-stage
#
# CI calls this once per release target, then `npm publish` each result
# and the main package. Locally it feeds packaging/npm/smoke-node.sh.
set -euo pipefail

os="$1" cpu="$2" napi_lib="$3" ffi_lib="$4" outdir="$5"
version="$(grep -m1 '"version"' "$(dirname "$0")/../../bindings/node/package.json" | sed 's/.*: "\(.*\)".*/\1/')"

ext=so; [ "$os" = darwin ] && ext=dylib

pkg="$outdir/kevy-node-$os-$cpu"
mkdir -p "$pkg"
cp "$napi_lib" "$pkg/kevy.node"
cp "$ffi_lib" "$pkg/libkevy_ffi.$ext"

cat > "$pkg/package.json" <<EOF
{
  "name": "@goliapkg/kevy-node-$os-$cpu",
  "version": "$version",
  "description": "kevy native engine for $os-$cpu. Install @goliapkg/kevy-node instead of this directly.",
  "license": "(Apache-2.0 OR MIT)",
  "repository": { "type": "git", "url": "git+https://github.com/goliajp/kevy.git" },
  "os": ["$os"],
  "cpu": ["$cpu"],
  "files": ["kevy.node", "libkevy_ffi.$ext"]
}
EOF

echo "staged $pkg (version $version)"
