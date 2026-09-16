#!/usr/bin/env bash
# The kevy-bin install smoke: stage this host's platform package from real
# binaries, pack it and the launcher, install both into a scratch project
# from the tarballs (no registry), and prove both bins run through the shim
# and report the launcher's version.
#
# The packed platform tarball is left in <stage-dir>. The release workflow
# publishes that file, so what reaches npm is the exact bytes this smoke
# installed and ran.
#
#   packaging/npm/smoke.sh <kevy> <kevy-cli> <stage-dir>
set -euo pipefail

kevy_bin="$1" cli_bin="$2" stage="$3"
here="$(cd "$(dirname "$0")" && pwd)"

# npm ships next to node; non-interactive shells often have node on PATH
# through a version manager but not its bin dir. Make both resolvable.
PATH="$(dirname "$(command -v node)"):$PATH"

os="$(node -p 'process.platform')"
cpu="$(node -p 'process.arch')"
version="$(node -p "require('$here/kevy-bin/package.json').version")"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$stage" "$scratch/project"
stage="$(cd "$stage" && pwd)"

"$here/gen-platform-pkg.sh" "$os" "$cpu" "$kevy_bin" "$cli_bin" "$scratch"
platform_tgz="$stage/goliapkg-kevy-bin-$os-$cpu-$version.tgz"
(cd "$scratch/kevy-bin-$os-$cpu" && npm pack --silent --pack-destination "$stage" > /dev/null)
[ -f "$platform_tgz" ] || { echo "FAIL: npm pack did not produce $platform_tgz"; exit 1; }

# The launcher points its optionalDependency at the local tarball — only in
# the scratch copy — so the install resolves the way a registry install
# does, offline. file: on a directory would symlink and change resolution.
cp -R "$here/kevy-bin" "$scratch/meta"
node -e '
  const fs = require("fs");
  const [p, name, spec] = process.argv.slice(1);
  const pkg = JSON.parse(fs.readFileSync(p, "utf8"));
  pkg.optionalDependencies = { [name]: spec };
  fs.writeFileSync(p, JSON.stringify(pkg, null, 2));
' "$scratch/meta/package.json" "@goliapkg/kevy-bin-$os-$cpu" "file:$platform_tgz"
(cd "$scratch/meta" && npm pack --silent --pack-destination "$scratch" > /dev/null)

cd "$scratch/project"
npm init -y --silent > /dev/null
npm install --silent --no-audit --no-fund "$platform_tgz"
npm install --silent --no-audit --no-fund "$scratch/goliapkg-kevy-bin-$version.tgz"

got="$(npx --no-install kevy --version)"
got_cli="$(npx --no-install kevy-cli --version)"
echo "kevy      -> $got"
echo "kevy-cli  -> $got_cli"
[ "$got" = "kevy $version" ] || { echo "FAIL: expected 'kevy $version'"; exit 1; }
[ "$got_cli" = "kevy-cli $version" ] || { echo "FAIL: expected 'kevy-cli $version'"; exit 1; }
echo "npm-smoke: ok ($os-$cpu, $platform_tgz)"
