#!/usr/bin/env bash
# Build the per-platform npm packages that @goliapkg/kevy-bin execs.
#
# The esbuild pattern: `@goliapkg/kevy-bin` declares one optionalDependency
# per (os, cpu) pair and ships only a launcher; npm installs exactly the
# matching platform package and `bin/resolve.js` execs the real binary out
# of it. The binary IS the package — no postinstall, no network fetch.
#
# That means the launcher is unpublishable on its own: without these three
# packages on the registry, `npx kevy` resolves nothing and dies with
# "kevy-bin-<platform> is not installed". They are built here, from the
# binaries a release already attached to its GitHub Release, so the npm
# artifact is byte-identical to the tarball users download by hand.
#
#   packaging/npm/build-platform-packages.sh <version> [outdir]
#
# <version> is the release version without the `v` (e.g. 4.0.0).
# Default outdir: target/npm-platform.
#
# Each package lands as:
#   <outdir>/kevy-bin-<os>-<cpu>/{package.json,kevy,kevy-cli,README.md,LICENSE-*}
#
# Publish them BEFORE the launcher, or the launcher's optionalDependencies
# point at versions the registry does not have yet.
set -euo pipefail

V=${1:?usage: build-platform-packages.sh <version> [outdir]}
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT=${2:-$ROOT/target/npm-platform}
REPO=${KEVY_REPO:-goliajp/kevy}

command -v gh >/dev/null || { echo "gh CLI required" >&2; exit 1; }

# triple:npm-os:npm-cpu — the (os, cpu) pairs must match bin/resolve.js's
# PLATFORMS table exactly, or a matching install still finds nothing.
TARGETS="
x86_64-unknown-linux-gnu:linux:x64
aarch64-unknown-linux-gnu:linux:arm64
aarch64-apple-darwin:darwin:arm64
"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$OUT"

echo "kevy npm platform packages — v$V"
for spec in $TARGETS; do
  triple=${spec%%:*}; rest=${spec#*:}; npmos=${rest%%:*}; npmcpu=${rest##*:}
  pkg="kevy-bin-$npmos-$npmcpu"
  tarball="kevy-v$V-$triple.tar.gz"

  gh release download "v$V" -R "$REPO" -p "$tarball" -D "$TMP" --clobber
  tar -xzf "$TMP/$tarball" -C "$TMP"
  src="$TMP/kevy-v$V-$triple"
  [ -x "$src/kevy" ] && [ -x "$src/kevy-cli" ] \
    || { echo "  $tarball: missing kevy or kevy-cli" >&2; exit 1; }

  dst="$OUT/$pkg"
  rm -rf "$dst"; mkdir -p "$dst"
  # resolve.js does path.join(packageRoot, "kevy"), so the binaries sit at
  # the package root — not under bin/.
  cp "$src/kevy" "$src/kevy-cli" "$dst/"
  cp "$src/LICENSE-MIT" "$src/LICENSE-APACHE" "$dst/" 2>/dev/null || true
  chmod +x "$dst/kevy" "$dst/kevy-cli"

  cat > "$dst/package.json" <<JSON
{
  "name": "@goliapkg/$pkg",
  "version": "$V",
  "description": "The kevy server and CLI binaries for $npmos $npmcpu. Installed automatically by @goliapkg/kevy-bin; not meant to be depended on directly.",
  "license": "MIT OR Apache-2.0",
  "repository": { "type": "git", "url": "git+https://github.com/$REPO.git" },
  "homepage": "https://kevy.golia.jp",
  "os": ["$npmos"],
  "cpu": ["$npmcpu"],
  "files": ["kevy", "kevy-cli", "LICENSE-MIT", "LICENSE-APACHE"],
  "preferUnplugged": true
}
JSON

  cat > "$dst/README.md" <<MD
# @goliapkg/$pkg

Prebuilt \`kevy\` and \`kevy-cli\` binaries for **$npmos $npmcpu**, from the
[v$V release](https://github.com/$REPO/releases/tag/v$V).

You do not install this package directly. \`@goliapkg/kevy-bin\` declares it
as an optional dependency; your package manager picks the one matching your
platform and the launcher execs the binary out of it.

\`\`\`sh
npm install -g @goliapkg/kevy-bin
kevy --port 6379
\`\`\`

Building from source instead: \`cargo install kevy kevy-cli\`.
MD

  size=$(du -sh "$dst" | cut -f1)
  echo "  $pkg  ($size)"
done

echo "built in $OUT"
echo "publish order: the three platform packages first, then @goliapkg/kevy-bin"
