#!/usr/bin/env bash
# replaymemgate — streaming replay's memory promise, measured.
#
# A v2 AOF streams record-by-record at open: peak RSS must be O(largest
# record), NOT O(file). A production 2.2 GB v1 log replayed at >1 GiB
# peak and OOM-looped a 1 GiB container; this gate builds a sizable v2
# log with crash_writer, then measures crash_check's whole-process peak
# RSS (via /usr/bin/time) — replay included — and fails if it exceeds
# MAX_RSS_MB (default 192 MiB, an order of magnitude under the file).
#
#   bash bench/replaymemgate.sh [target_mb] [max_rss_mb]
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_MB=${1:-512}
MAX_RSS_MB=${2:-192}

cargo build -q -p kevy-embedded --example crash_writer --example crash_check || exit 1
WRITER="$HERE/target/debug/examples/crash_writer"
CHECK="$HERE/target/debug/examples/crash_check"

WORK=$(mktemp -d /tmp/replaymem.XXXXXX)
WPID=""
cleanup() { [ -n "$WPID" ] && kill -9 "$WPID" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

dir="$WORK/db"
mkdir -p "$dir"
echo "replaymemgate: growing a ~${TARGET_MB}MB v2 AOF…"
"$WRITER" "$dir" > "$WORK/w.log" 2>/dev/null &
WPID=$!
aof="$dir/aof-0.aof"
for _ in $(seq 600); do
    size=$(stat -f%z "$aof" 2>/dev/null || stat -c%s "$aof" 2>/dev/null || echo 0)
    [ "$size" -ge $((TARGET_MB * 1024 * 1024)) ] && break
    sleep 1
done
kill -9 "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null; WPID=""
size=$(stat -f%z "$aof" 2>/dev/null || stat -c%s "$aof")
echo "replaymemgate: AOF = $((size / 1024 / 1024))MB"

# Peak RSS of the reopen (replay + valid-prefix + report), portable field
# scrape: macOS `time -l` prints bytes, Linux `time -v` prints KB.
if /usr/bin/time -l true 2>/dev/null >/dev/null; then
    out=$(/usr/bin/time -l "$CHECK" "$dir" 2>&1 >/dev/null)
    rss_bytes=$(echo "$out" | awk '/maximum resident set size/{print $1}')
else
    out=$(/usr/bin/time -v "$CHECK" "$dir" 2>&1 >/dev/null)
    rss_bytes=$(( $(echo "$out" | awk -F: '/Maximum resident set size/{print $2}' | tr -d ' ') * 1024 ))
fi
rss_mb=$((rss_bytes / 1024 / 1024))
echo "replaymemgate: replay peak RSS = ${rss_mb}MB (file $((size / 1024 / 1024))MB, cap ${MAX_RSS_MB}MB)"
if [ "$rss_mb" -le "$MAX_RSS_MB" ]; then
    echo "replaymemgate: PASS"
else
    echo "replaymemgate: FAIL — streaming replay must not scale RSS with file size"
    exit 1
fi
