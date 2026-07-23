#!/usr/bin/env bash
# embeddedgate C driver. Compiles LMDB 0.9.33 (vendored, self-contained — no
# system install) + the harness, links the kevy engine cdylib (release), runs.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

cargo build --release -p kevy-ffi --manifest-path "$ROOT/Cargo.toml" >/dev/null

clang -O2 -o "$HERE/embgate-c" \
  "$HERE/main.c" "$HERE/vendor/lmdb/mdb.c" "$HERE/vendor/lmdb/midl.c" \
  -I "$HERE/vendor/lmdb" -I "$ROOT/crates/kevy-ffi/include" \
  "$ROOT/target/release/libkevy_ffi.dylib" \
  -lpthread

DYLD_LIBRARY_PATH="$ROOT/target/release" "$HERE/embgate-c"
rm -f "$HERE/embgate-c"
