#!/usr/bin/env bash
# embeddedgate Go driver. kevy-go's cgo preamble links
# target/debug/libkevy_ffi.a; for a fair perf comparison against optimized
# bbolt/badger we stage the RELEASE static lib at that path, run, and restore
# the debug lib after (so a later `cargo test` is unaffected).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DBG="$ROOT/target/debug/libkevy_ffi.a"
REL="$ROOT/target/release/libkevy_ffi.a"

cargo build --release -p kevy-ffi --manifest-path "$ROOT/Cargo.toml" >/dev/null

BACKUP=""
if [ -f "$DBG" ]; then
  BACKUP="$(mktemp)"
  cp "$DBG" "$BACKUP"
fi
restore() {
  if [ -n "$BACKUP" ]; then cp "$BACKUP" "$DBG"; rm -f "$BACKUP"; fi
}
trap restore EXIT

mkdir -p "$(dirname "$DBG")"
cp "$REL" "$DBG"

cd "$ROOT/bench/embeddedgate/go"
go mod tidy >/dev/null 2>&1 || true
# The tag is what links the engine in; without it this harness would
# build the pure-Go remote client and have no engine to measure.
go run -tags kevy_embedded .
