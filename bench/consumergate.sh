#!/usr/bin/env bash
# consumergate — build and run tools/facadegate from consumer position.
#
# The gate's whole value is WHERE it stands: its own workspace, its own
# lockfile, facade imports only. Run from inside the workspace it would
# prove nothing — kevy-embedded shipped 4.0 with the TABLE face
# uncallable (dogfood F7) precisely because every in-workspace build
# resolves internal paths the facade never exported.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
cd "$HERE/tools/facadegate"
cargo run --quiet
