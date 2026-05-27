#!/usr/bin/env bash
# Drive the kevy-uring cross-language perf bench. Linux-only — io_uring
# does not exist on macOS / Windows. Emits one JSON line per (competitor,
# workload). Caller redirects to results-<date>.jsonl.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

if [ "$(uname -s)" != "Linux" ]; then
  echo "kevy-uring bench requires Linux (io_uring); skipping on $(uname -s)." >&2
  exit 0
fi

# Rust kevy-uring
(cd "$HERE/rust" && CARGO_TARGET_DIR=./target cargo build --release 2>&1 >&2)
"$HERE/rust/target/release/kevy-uring-comparative-bench"

# C liburing
(cd "$HERE/c" && make 2>&1 >&2)
"$HERE/c/bench"
