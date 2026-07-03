#!/bin/bash
# diskgate — the disk-growth + persistence-latency gate (v2.1 五轴收口
# disk axis).
#
#   bash bench/diskgate.sh <KEVY_BIN>                    # gate
#   bash bench/diskgate.sh <KEVY_BIN> --update-baseline  # re-record
#
# First lines (v2.1): AOF bytes/op for plain SETs, and BGREWRITEAOF
# wall time at N keys. The v2.3 offset-spine train replaces the
# rewrite line with the 30M-key stall + replay-throughput bounds it
# must state (docs/persistence.md); feed-retention lines attach when
# CDC lands. Official runs on lx64.
#
# Exit codes: 0 = PASS, 1 = bound violated, 2 = refused.
set -u

BIN=${1:?usage: diskgate.sh <KEVY_BIN> [--update-baseline]}
MODE=${2:-gate}
HERE=$(cd "$(dirname "$0")" && pwd)
BASELINE="$HERE/DISK-BASELINE.json"
PORT=${DISKGATE_PORT:-6298}
N=${DISKGATE_N:-100000}
BAND_PCT=${BAND_PCT:-20}

command -v redis-benchmark >/dev/null 2>&1 || { echo "diskgate: redis-benchmark not found" >&2; exit 2; }

DIR=$(mktemp -d)
mkdir -p "$DIR/data"
"$BIN" --port "$PORT" --threads 1 --dir "$DIR/data" &> "$DIR/server.log" &
SRV=$!
trap 'kill $SRV 2>/dev/null; sleep 0.2; kill -9 $SRV 2>/dev/null; wait $SRV 2>/dev/null; rm -rf "$DIR"' EXIT
sleep 1

redis-benchmark -p "$PORT" -t set -n "$N" -r "$N" -d 64 -q >/dev/null 2>&1
AOF_BYTES=$(find "$DIR/data" -name '*.aof' -exec stat -f%z {} + 2>/dev/null || find "$DIR/data" -name '*.aof' -exec stat -c%s {} + 2>/dev/null)
AOF_TOTAL=$(echo "$AOF_BYTES" | awk '{s+=$1} END {print s+0}')
BPO=$(( AOF_TOTAL / N ))

T0=$(python3 -c 'import time; print(time.time())')
redis-cli -p "$PORT" bgrewriteaof >/dev/null
# Poll until rewrite finishes (INFO persistence aof_rewrite_in_progress).
for _ in $(seq 1 120); do
    IP=$(redis-cli -p "$PORT" info persistence 2>/dev/null | tr -d '\r' | awk -F: '/aof_rewrite_in_progress/{print $2}')
    [ "${IP:-0}" = "0" ] && break
    sleep 0.25
done
REWRITE_MS=$(python3 -c "import time; print(int((time.time()-$T0)*1000))")
echo "diskgate: AOF bytes/op=$BPO, rewrite@${N}keys=${REWRITE_MS}ms"

if [ "$MODE" = "--update-baseline" ]; then
    printf '{\n  "aof_bytes_per_set_d64": %s,\n  "rewrite_ms_at_%s_keys": %s,\n  "band_pct": %s,\n  "recorded": "%s"\n}\n' \
        "$BPO" "$N" "$REWRITE_MS" "$BAND_PCT" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$BASELINE"
    echo "diskgate: baseline recorded"
    exit 0
fi

[ -f "$BASELINE" ] || { echo "diskgate: no baseline — run --update-baseline on lx64 first" >&2; exit 2; }
FAIL=0
BASE_BPO=$(python3 -c "import json; print(json.load(open('$BASELINE'))['aof_bytes_per_set_d64'])")
OK=$(python3 -c "b=$BASE_BPO; print(1 if $BPO <= b + b*$BAND_PCT/100 else 0)")
[ "$OK" = "1" ] || { echo "diskgate: FAIL — AOF bytes/op $BPO > baseline $BASE_BPO +${BAND_PCT}%" >&2; FAIL=1; }
BASE_RW=$(python3 -c "import json,sys; d=json.load(open('$BASELINE')); print(d.get('rewrite_ms_at_${N}_keys', 0))")
if [ "${BASE_RW:-0}" != "0" ]; then
    OK=$(python3 -c "b=$BASE_RW; print(1 if $REWRITE_MS <= b*2 else 0)")
    [ "$OK" = "1" ] || { echo "diskgate: FAIL — rewrite ${REWRITE_MS}ms > 2x baseline ${BASE_RW}ms" >&2; FAIL=1; }
fi
# v2.3: recovery-point contract line — snapshot + feed replay must
# reconstruct exact state (docs/cdc.md).
if [ "$FAIL" = "0" ]; then
    bash "$(dirname "$0")/restore-drill.sh" "$BIN" || { echo "diskgate: FAIL — restore drill" >&2; exit 1; }
    echo "diskgate: PASS"
fi
exit "$FAIL"
