#!/usr/bin/env bash
# allocgate-prep — the two binaries the allocgate family compares.
#
# The gates A/B a default build against `--features kevy-alloc` (the
# whole process routed through kevy-alloc). Both go through the same
# target dir, so each build overwrites the other's output; this stages
# them under target/allocgate/ and prints export lines for the caller:
#
#   eval "$(bash bench/allocgate-prep.sh)"
#   bash bench/allocgate.sh
#
# Rebuilds only when the staged copy is older than the workspace's
# newest commit-relevant source — cheap enough to be honest, cached
# enough to not double every full-tier run.
set -eu
cd "$(dirname "$0")/.."

STAGE=target/allocgate
mkdir -p "$STAGE"

build() { # $1 = off|on, $2 = extra cargo args
    local out="$STAGE/kevy-$1"
    if [ ! -x "$out" ] || [ -n "$(find crates -name '*.rs' -newer "$out" -print -quit 2>/dev/null)" ]; then
        # shellcheck disable=SC2086
        cargo build --release -p kevy --bin kevy $2 >&2
        cp target/release/kevy "$out"
    fi
}

build off ""
build on "--features kevy-alloc"

echo "export ALLOCGATE_BIN_OFF=$PWD/$STAGE/kevy-off"
echo "export ALLOCGATE_BIN_ON=$PWD/$STAGE/kevy-on"
