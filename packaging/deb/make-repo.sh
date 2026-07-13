#!/usr/bin/env bash
# Assemble a flat signed APT repository from built .debs. Static files
# only — rsync the result to any web host and clients add:
#
#   deb [signed-by=/usr/share/keyrings/kevy.gpg] https://kevy.golia.jp/apt ./
#
# Usage:
#   packaging/deb/make-repo.sh <debs-dir> <repo-outdir> <gpg-key-id>
#
# Needs dpkg-scanpackages (dpkg-dev) and gpg with the signing key loaded.
set -euo pipefail

debs="$1" out="$2" key="$3"

mkdir -p "$out"
cp "$debs"/*.deb "$out/"

(cd "$out" && dpkg-scanpackages --multiversion . > Packages)
gzip -kf "$out/Packages"

(cd "$out" && {
  echo "Origin: kevy"
  echo "Label: kevy"
  echo "Suite: stable"
  echo "Architectures: amd64 arm64"
  echo "Date: $(date -Ru)"
  echo "SHA256:"
  for f in Packages Packages.gz; do
    printf ' %s %s %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$(stat -c %s "$f")" "$f"
  done
} > Release)

gpg --default-key "$key" -abs -o "$out/Release.gpg" "$out/Release"
gpg --default-key "$key" --clearsign -o "$out/InRelease" "$out/Release"
gpg --export "$key" > "$out/kevy.gpg"

echo "repo staged at $out"
