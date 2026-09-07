# Shared competitor-anchor helpers. Source this; do not run it.
#
# Every bench script that measures kevy against something else answers the
# same question first: which version of that something. The answer lives in
# bench/COMPETITOR-ANCHORS.json, and it is asked of the running engine
# rather than of a comment, because a comment cannot be wrong out loud. The
# 2026-09-01 arena table is the reason: it named the redis image by bare
# major, docker served a cached 8.10.0 while the registry served 8.10.1,
# and the published ratios recorded neither.
#
#   anchor_pin <name>              -> the pinned version string
#   anchor_image_ver <docker-args> -> version reported by a container image
#   anchor_bin_ver <binary>        -> version reported by a local binary
#   anchor_require <label> <pinned> <reported>  -> exits 1 on mismatch
#
# tools/check_competitor_anchors.py keeps the pins level with each
# project's latest stable release, and fails when they drift or go stale.

_ANCHOR_JSON="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/COMPETITOR-ANCHORS.json"

anchor_pin() {
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["anchors"][sys.argv[2]]["pinned"])' \
        "$_ANCHOR_JSON" "$1"
}

# redis/valkey answer `v=8.10.1`; dragonfly answers `v1.40.2-<sha>` in colour.
_anchor_ver_of() {
    sed -E 's/\x1b\[[0-9;]*m//g' | grep -oE "v=[0-9][0-9.]*|v[0-9]+\.[0-9]+\.[0-9]+" | head -1 | tr -d 'v='
}

anchor_image_ver() { docker run --rm "$@" 2>&1 | _anchor_ver_of; }

anchor_bin_ver() { "$@" --version 2>&1 | _anchor_ver_of; }

anchor_require() { # label, pinned, reported
    if [ "$2" != "$3" ]; then
        echo "!! $1: reports '${3:-nothing}' but COMPETITOR-ANCHORS.json pins $2" >&2
        echo "!! refusing to produce numbers against an engine version that is not on record" >&2
        exit 1
    fi
}
