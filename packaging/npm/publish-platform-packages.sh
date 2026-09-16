#!/usr/bin/env bash
# Publish the npm platform packages, then the kevy-bin launcher.
#
# Two main packages resolve their native half out of a platform package:
# @goliapkg/kevy-node (and kevy-ts, which pins the same three) and
# @goliapkg/kevy-bin. Until 6.4.0 no workflow published any platform
# package. kevy-node's three sat at 5.1.0 while every kevy-node from 6.0.0
# on pinned its own version, so `npm install @goliapkg/kevy-node` succeeded,
# skipped the optional dependency it could not find, and failed at load.
# kevy-bin had never been published at all. Every gate was green, because
# the channel check derived doors from bindings/ and a generated package is
# not in the tree.
#
#   packaging/npm/publish-platform-packages.sh <version> <stage-dir>
#
# <stage-dir> holds the tarballs smoke.sh and smoke-node.sh installed and
# ran on each target. The packages owed are READ from the main packages'
# optionalDependencies, not listed here: a platform added there and not
# staged is a refusal before anything is published.
set -euo pipefail

V=${1:?usage: publish-platform-packages.sh <version> <stage-dir>}
STAGE=${2:?usage: publish-platform-packages.sh <version> <stage-dir>}
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MAINS="bindings/node/package.json packaging/npm/kevy-bin/package.json"

owed=$(cd "$ROOT" && node -e '
  const v = process.argv[1];
  const bad = [];
  const out = new Set();
  for (const f of process.argv.slice(2)) {
    const p = require("./" + f);
    if (p.version !== v) bad.push(`${f} is ${p.version}, not ${v}`);
    for (const [n, want] of Object.entries(p.optionalDependencies || {})) {
      if (want !== v) bad.push(`${f} pins ${n}@${want}, not ${v}`);
      out.add(n);
    }
  }
  if (bad.length) { console.error(bad.join("\n")); process.exit(1); }
  console.log([...out].sort().join("\n"));
' "$V" $MAINS)
n_owed=$(printf '%s\n' "$owed" | grep -c .)
[ "$n_owed" -ge 6 ] || { echo "only $n_owed platform package(s) owed — the manifests were misread"; exit 1; }

# Every owed package must be staged, with the files its loader opens,
# before any of them is published. A partial set on the registry is the
# failure this script exists to end.
for name in $owed; do
  short=${name#@goliapkg/}
  tgz="$STAGE/goliapkg-$short-$V.tgz"
  [ -f "$tgz" ] || { echo "REFUSING: $name is owed but $tgz was not staged"; exit 1; }
  got=$(tar -xOzf "$tgz" package/package.json | node -p 'const p=JSON.parse(require("fs").readFileSync(0,"utf8")); p.name+"@"+p.version')
  [ "$got" = "$name@$V" ] || { echo "REFUSING: $tgz says $got"; exit 1; }
  listing=$(tar -tzvf "$tgz")
  has() {
    printf '%s\n' "$listing" | grep -Eq "$1" \
      || { echo "REFUSING: $tgz has nothing matching $1"; exit 1; }
  }
  case "$short" in
    # exec'd by bin/resolve.js, so the mode matters as much as the file
    kevy-bin-*)  has '^-rwx.* package/kevy$'; has '^-rwx.* package/kevy-cli$' ;;
    # dlopen'd by node.js and bun.js
    kevy-node-*) has '^-.* package/kevy\.node$'; has '^-.* package/libkevy_ffi\.(so|dylib)$' ;;
    *) echo "REFUSING: no file rule for $name"; exit 1 ;;
  esac
done
echo "staged all $n_owed owed platform packages at $V"

publish() { # <name> <tarball or dir>
  if npm view "$1@$V" version >/dev/null 2>&1; then
    echo "$1@$V already published"
  else
    npm publish "$2" --access public
    echo "published $1@$V"
  fi
}

for name in $owed; do
  short=${name#@goliapkg/}
  publish "$name" "$STAGE/goliapkg-$short-$V.tgz"
done

# The launcher last: published first, it names versions that do not exist.
# kevy-node itself is published by release.yml's binding loop, which runs
# after this job for the same reason.
publish "@goliapkg/kevy-bin" "$ROOT/packaging/npm/kevy-bin"
