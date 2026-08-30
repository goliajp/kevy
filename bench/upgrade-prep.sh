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

# The previous release, asked of crates.io rather than computed. The
# arithmetic that used to live here — major.(minor-1).0 — is right for
# 5.3.x against 5.2.0 and wrong for the first release of any major line:
# at workspace 6.0.0 it produced `6.-1.0`, cargo refused the version, and
# upgrade-interop failed with "a-pri never came up" — a message about a
# server, for a version string that never existed. A major bump is exactly
# when mixed-version interop most wants checking, and it was exactly when
# this could not run.
WS=$(grep -m1 '^version' Cargo.toml | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
PREV=$(python3 - "$WS" <<'PREVPY'
import json, sys, urllib.request
ws = tuple(int(x) for x in sys.argv[1].split("."))
req = urllib.request.Request("https://crates.io/api/v1/crates/kevy",
                             headers={"User-Agent": "kevy-upgrade-prep"})
with urllib.request.urlopen(req, timeout=30) as r:
    vs = json.load(r)["versions"]
cand = []
for v in vs:
    if v.get("yanked"):
        continue
    try:
        t = tuple(int(x) for x in v["num"].split("."))
    except ValueError:
        continue
    if len(t) == 3 and t < ws:
        cand.append(t)
if not cand:
    sys.exit("upgrade-prep: crates.io has no released version below " + sys.argv[1])
print("%d.%d.%d" % max(cand))
PREVPY
) || exit 1

echo "export UPGRADE_OLD_BIN=$(stage 4.1.1)"
echo "export UPGRADE_PREV_BIN=$(stage "$PREV")"
