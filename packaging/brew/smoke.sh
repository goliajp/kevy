#!/usr/bin/env bash
# The Homebrew packaging smoke: stage a real host tarball, generate the
# formula against it, audit it, and (opt-in) actually install it from source
# into the local brew and run the installed binaries.
#
# `brew audit --strict` is the always-on check — it catches the formula-style
# regressions that break a tap PR. The install leg is opt-in
# (KEVY_BREW_SMOKE_INSTALL=1) because it mutates the host's real Homebrew
# Cellar (brew has no per-invocation prefix sandbox); it installs from a
# local file:// tarball (no network) and uninstalls itself afterward.
#
# Usage:
#   packaging/brew/smoke.sh <version> <kevy-bin> <kevy-cli-bin> <scratch-dir>
#   e.g. smoke.sh 4.0.0 target/release/kevy target/release/kevy-cli /tmp/kevy-brew-smoke
set -euo pipefail

v="$1" kevy_bin="$2" cli_bin="$3" scratch="$4"
here="$(cd "$(dirname "$0")" && pwd)"

if ! command -v brew >/dev/null 2>&1; then
    echo "brew-smoke: SKIP — brew not found (Homebrew packaging smoke; run on a host with Homebrew)"
    exit 0
fi

# The formula only carries three slots: darwin/arm, linux/intel, linux/arm.
# Map the host to one; anything else (e.g. Intel mac) has no url to test.
os="$(uname -s)"; machine="$(uname -m)"
case "$os/$machine" in
    Darwin/arm64)        target="aarch64-apple-darwin" ;;
    Linux/x86_64)        target="x86_64-unknown-linux-gnu" ;;
    Linux/aarch64|Linux/arm64) target="aarch64-unknown-linux-gnu" ;;
    *) echo "brew-smoke: SKIP — $os/$machine has no matching formula slot"; exit 0 ;;
esac

rm -rf "$scratch"
mkdir -p "$scratch/staging"

# 1. Stage the release tarball exactly as .github/workflows/release.yml does:
#    a top-level kevy-v<ver>-<target>/ dir the formula's install globs for.
stage="kevy-v$v-$target"
sdir="$scratch/staging/$stage"
mkdir -p "$sdir"
install -m 0755 "$kevy_bin" "$sdir/kevy"
install -m 0755 "$cli_bin" "$sdir/kevy-cli"
cp README.md LICENSE-APACHE LICENSE-MIT "$sdir/" 2>/dev/null || true
tar -czf "$scratch/staging/$stage.tar.gz" -C "$scratch/staging" "$stage"
rm -rf "$sdir"

sha="$(shasum -a 256 "$scratch/staging/$stage.tar.gz" | awk '{print $1}')"

# 2. Generate the formula pointing at the local staging dir. Only the host's
#    slot is ever fetched here; the other two carry the same (valid, 64-hex)
#    sha so `brew audit` — which is offline under --strict — stays happy.
#
# Modern brew disables `brew audit <path>` and `brew install <path>`: a
# formula must live in a tap by name. Stage it into a throwaway local tap,
# audit/install by name, and untap on exit.
tap="kevysmoke/local"
tapdir="$(brew --repository)/Library/Taps/kevysmoke/homebrew-local"
if [ -e "$tapdir" ]; then
    echo "brew-smoke: SKIP — temp tap $tap already exists; remove it first (brew untap $tap)"
    exit 0
fi
srvpid=""
cleanup() {
    [ -n "$srvpid" ] && kill "$srvpid" 2>/dev/null || true
    brew uninstall --force kevy >/dev/null 2>&1 || true
    brew untap "$tap" >/dev/null 2>&1 || true
}
trap cleanup EXIT

brew tap-new --no-git "$tap" >/dev/null
formula="$tapdir/Formula/kevy.rb"
mkdir -p "$tapdir/Formula"
"$here/gen-formula.sh" "$v" "$sha" "$sha" "$sha" "file://$scratch/staging" > "$formula"

# 3. Audit — the always-on check.
echo "brew-smoke: auditing $tap/kevy …"
brew audit --strict "$tap/kevy"
echo "brew-smoke: audit OK"

# 4. Install leg — opt-in, mutates the real Cellar, self-cleans via trap.
if [ "${KEVY_BREW_SMOKE_INSTALL:-0}" != "1" ]; then
    echo "brew-smoke: install leg skipped (set KEVY_BREW_SMOKE_INSTALL=1 to install+run locally)"
    echo "brew-smoke: ok (audit)"
    exit 0
fi

if brew list kevy >/dev/null 2>&1; then
    echo "brew-smoke: SKIP install — a 'kevy' is already installed; not touching it"
    echo "brew-smoke: ok (audit)"
    exit 0
fi

echo "brew-smoke: installing from source (local tarball) …"
brew install --build-from-source "$tap/kevy"

kbin="$(brew --prefix)/bin/kevy"
cbin="$(brew --prefix)/bin/kevy-cli"
echo "kevy      -> $("$kbin" --version)"
echo "kevy-cli  -> $("$cbin" --version)"

# Real round trip through the installed binaries.
port="${KEVY_BREW_SMOKE_PORT:-7532}"
data="$scratch/data"
env KEVY_BIND=127.0.0.1 "$kbin" --port "$port" --dir "$data" > "$scratch/server.log" 2>&1 &
srvpid=$!
for _ in $(seq 100); do "$cbin" -p "$port" PING >/dev/null 2>&1 && break; sleep 0.1; done
pong="$("$cbin" -p "$port" PING 2>/dev/null || true)"
case "$pong" in PONG|+PONG) ;; *) echo "brew-smoke: FAIL — no PONG (got '$pong')"; tail -5 "$scratch/server.log"; exit 1 ;; esac

echo "brew-smoke: ok (audit + install + run)"
