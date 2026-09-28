#!/usr/bin/env bash
# mirigate — the unsafe stones under miri: slot management in kevy-map and
# kevy-bytes, the SPSC ring's atomics, and the zset encoding in kevy-store.
# CI's miri job and the suite both run this file.
set -euo pipefail
cd "$(dirname "$0")/.."

rustup component list --toolchain nightly --installed 2>/dev/null | grep -q '^miri' || {
  echo "mirigate: REFUSED — no miri (rustup component add miri rust-src --toolchain nightly)" >&2
  exit 2
}
export MIRIFLAGS="-Zmiri-disable-isolation"
cargo +nightly miri test -p kevy-map --lib
cargo +nightly miri test -p kevy-bytes --lib
# The two-thread stress tests run with a cfg(miri)-reduced N (2k iterations
# instead of 200k/1M): the race detector needs the full/empty boundary
# crossings, not the iteration count.
cargo +nightly miri test -p kevy-ring --lib
# The zset subset only — encoding roundtrip, skiplist/listpack switch, score
# ordering. The full kevy-store lib suite is not miri-runnable: its large-N
# tests (snapshot, memory pause and accounting sweeps) run for hours under
# miri's ~1000x interpretation overhead.
cargo +nightly miri test -p kevy-store --lib zset
echo "mirigate: PASS"
