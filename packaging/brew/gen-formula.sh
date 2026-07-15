#!/usr/bin/env bash
# Emit the Homebrew formula for one release. CI runs this after the release
# assets exist, with the real sha256 of each tarball, and pushes the result
# to the goliajp/homebrew-kevy tap.
#
# Usage:
#   gen-formula.sh <version> <sha-darwin-arm64> <sha-linux-x64> <sha-linux-arm64> [base-url]
# base-url defaults to the GitHub release download path for v<version>.
set -euo pipefail

v="$1" sha_mac="$2" sha_lx="$3" sha_la="$4"
base="${5:-https://github.com/goliajp/kevy/releases/download/v$v}"

cat <<EOF
class Kevy < Formula
  desc "Redis-compatible data layer for AI systems — server + cli"
  homepage "https://kevy.golia.jp"
  license any_of: ["Apache-2.0", "MIT"]

  on_macos do
    on_arm do
      url "$base/kevy-v$v-aarch64-apple-darwin.tar.gz"
      sha256 "$sha_mac"
    end
  end

  on_linux do
    on_intel do
      url "$base/kevy-v$v-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$sha_lx"
    end
    on_arm do
      url "$base/kevy-v$v-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$sha_la"
    end
  end

  def install
    prefix_dir = Dir["kevy-v#{version}-*"].first || "."
    bin.install "#{prefix_dir}/kevy", "#{prefix_dir}/kevy-cli"
  end

  test do
    assert_match "kevy #{version}", shell_output("#{bin}/kevy --version")
    assert_match "kevy-cli #{version}", shell_output("#{bin}/kevy-cli --version")
  end
end
EOF
