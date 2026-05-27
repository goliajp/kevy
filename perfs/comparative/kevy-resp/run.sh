#!/usr/bin/env bash
# Drive each language's comparative bench program and concatenate their
# JSON-line output to stdout (the caller redirects to results-<date>.jsonl).
#
# Each language's program must:
#   - print one JSON object per line, matching the schema in
#     perfs/comparative/README.md
#   - exit 0 on success
#   - take no arguments (all parameters embedded in the program)
#
# Languages absent for a stone (e.g. no Go competitor) — just skip that
# block by deleting it or guarding with `if false`.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

run_lang() {
  local lang="$1"
  local dir="$HERE/$lang"
  [ -d "$dir" ] || return 0
  case "$lang" in
    rust)
      (cd "$dir" && cargo build --release 2>&1 >&2)
      "$dir/target/release/bench"
      ;;
    go)
      (cd "$dir" && go build -o bench ./... 2>&1 >&2)
      "$dir/bench"
      ;;
    c|cpp)
      (cd "$dir" && make 2>&1 >&2)
      "$dir/bench"
      ;;
  esac
}

for lang in rust go c cpp; do
  run_lang "$lang"
done
