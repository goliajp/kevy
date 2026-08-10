#!/usr/bin/env bash
# The npm packaging smoke: build the platform package from real binaries,
# install the meta package against it in a scratch project, and prove both
# bins run through the shim. This is what CI runs before any publish.
#
# Usage: packaging/npm/smoke.sh <kevy-bin> <kevy-cli-bin> <scratch-dir>
set -euo pipefail

kevy_bin="$1" cli_bin="$2" scratch="$3"
here="$(cd "$(dirname "$0")" && pwd)"

# npm ships next to node; non-interactive shells often have node on PATH
# through a version manager but not its bin dir. Make both resolvable.
PATH="$(dirname "$(command -v node)"):$PATH"

os="$(node -p 'process.platform')"
cpu="$(node -p 'process.arch')"

rm -rf "$scratch"
mkdir -p "$scratch/project"

"$here/gen-platform-pkg.sh" "$os" "$cpu" "$kevy_bin" "$cli_bin" "$scratch"

cd "$scratch/project"
npm init -y --silent > /dev/null

# Install from PACKED tarballs, not file: paths — npm symlinks file: deps,
# which changes module resolution; the registry ships tarballs that npm
# copies. The smoke must exercise exactly what a real install does, offline.
cp -R "$here/kevy-bin" "$scratch/kevy-bin-meta"
node -e '
  const fs = require("fs");
  const p = process.argv[1];
  const pkg = JSON.parse(fs.readFileSync(p, "utf8"));
  pkg.optionalDependencies = { [process.argv[2]]: process.argv[3] };
  fs.writeFileSync(p, JSON.stringify(pkg, null, 2));
' "$scratch/kevy-bin-meta/package.json" \
  "@goliapkg/kevy-bin-$os-$cpu" "file:$scratch/kevy-bin-$os-$cpu.tgz"
(cd "$scratch/kevy-bin-$os-$cpu" && npm pack --silent --pack-destination "$scratch" > /dev/null)
mv "$scratch"/goliapkg-kevy-bin-"$os"-"$cpu"-*.tgz "$scratch/kevy-bin-$os-$cpu.tgz"
(cd "$scratch/kevy-bin-meta" && npm pack --silent --pack-destination "$scratch" > /dev/null)
mv "$scratch"/goliapkg-kevy-bin-[0-9]*.tgz "$scratch/kevy-bin-meta.tgz"
npm install --silent --no-audit --no-fund "$scratch/kevy-bin-$os-$cpu.tgz"
npm install --silent --no-audit --no-fund "$scratch/kevy-bin-meta.tgz"

got="$(npx --no-install kevy --version)"
got_cli="$(npx --no-install kevy-cli --version)"
echo "kevy      -> $got"
echo "kevy-cli  -> $got_cli"
case "$got" in kevy\ *) ;; *) echo "FAIL: unexpected kevy --version"; exit 1 ;; esac
case "$got_cli" in kevy-cli\ *) ;; *) echo "FAIL: unexpected kevy-cli --version"; exit 1 ;; esac
echo "npm-smoke: ok"
