#!/usr/bin/env bash
# docgate — the API documentation builds without a single rustdoc warning.
#
# A link to a private item, a link to nothing, an unclosed HTML tag in a
# doc comment: each renders as dead text on docs.rs and none of them fails
# a build, a test or clippy. `-D warnings` makes every one fatal here.
#
# Every feature is on, so feature-gated items and their docs are built
# too — except kevy-client-async, whose three runtime features exclude
# each other by contract (its lib.rs refuses two at once); it is
# documented with its default runtime, the one docs.rs shows.
#
# Usage: bash bench/docgate.sh
set -euo pipefail
cd "$(dirname "$0")/.."
export RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings"

cargo doc --workspace --no-deps --all-features --exclude kevy-client-async
cargo doc -p kevy-client-async --no-deps
echo "docgate: ok — every workspace crate documents warning-free"
