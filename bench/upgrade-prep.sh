#!/usr/bin/env bash
# upgrade-prep — the published binaries the upgrade gates compare against.
#
# upgradegate proves the charter's L6 line (swapping the binary is the
# whole upgrade) against the 4.1.1 line; upgrade-interop proves the
# mixed-version pair across the previous minor boundary. Both need a
# real released binary, which this stages via cargo install into a
# per-version cache — network once per box, then durable.
#
#   eval "$(bash bench/upgrade-prep.sh)"
#   bash bench/upgradegate.sh "$UPGRADE_OLD_BIN" target/release/kevy
#   bash bench/upgrade-interop.sh "$UPGRADE_PREV_BIN" target/release/kevy
#
# PREV is the latest published minor before this tree's version, read
# from the workspace so a new release line does not need this file edited.
set -eu
cd "$(dirname "$0")/.."

CACHE="${UPGRADE_CACHE:-$HOME/.kevy-upgrade-cache}"

stage() { # $1 = version
    local root="$CACHE/$1"
    if [ ! -x "$root/bin/kevy" ]; then
        cargo install kevy --version "$1" --root "$root" --locked >&2 \
          || cargo install kevy --version "$1" --root "$root" >&2
    fi
    echo "$root/bin/kevy"
}

# The previous minor: workspace 5.3.x compares against 5.2.0, and so on.
WS=$(grep -m1 '^version' Cargo.toml | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
MINOR=$(echo "$WS" | cut -d. -f2)
PREV="$(echo "$WS" | cut -d. -f1).$((MINOR - 1)).0"

echo "export UPGRADE_OLD_BIN=$(stage 4.1.1)"
echo "export UPGRADE_PREV_BIN=$(stage "$PREV")"
