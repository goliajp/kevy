#!/bin/bash
# memgate — the memory-formula gate (v2.1 五轴收口 mem axis).
#
#   bash bench/memgate.sh <KEVY_BIN>                    # gate against formulas
#   bash bench/memgate.sh <KEVY_BIN> --update-baseline  # re-record measured values
#
# Contract (ratchet, same discipline as perfgate): every subsystem that
# claims a memory formula in its RFC gets a line here — the measured
# bytes/entry must sit within ±BAND_PCT of the formula. Official runs
# happen on lx64; the script itself is portable for mechanics testing.
#
# First lines (v2.1): plain-string keyspace bytes/entry at two value
# sizes, measured via INFO memory used_memory over a known key count.
# Index/view/text/vector formulas attach here as their trains land
# (v2.5+), one JSON entry per formula.
#
# Exit codes: 0 = PASS, 1 = formula violated, 2 = refused.
set -u

BIN=${1:?usage: memgate.sh <KEVY_BIN> [--update-baseline]}
MODE=${2:-gate}
HERE=$(cd "$(dirname "$0")" && pwd)
BASELINE="$HERE/MEM-BASELINE.json"
PORT=${MEMGATE_PORT:-6299}
N=${MEMGATE_N:-100000}
BAND_PCT=${BAND_PCT:-20}

command -v redis-benchmark >/dev/null 2>&1 || {
    echo "memgate: redis-benchmark not found" >&2
    exit 2
}

DIR=$(mktemp -d)
"$BIN" --port "$PORT" --threads 1 &> "$DIR/server.log" &
SRV=$!
trap 'kill $SRV 2>/dev/null; rm -rf "$DIR"' EXIT
sleep 1

measure() { # $1 = value size → echoes bytes/entry
    redis-benchmark -p "$PORT" -t set -n "$N" -r "$N" -d "$1" -q >/dev/null 2>&1
    local used keys
    used=$(redis-cli -p "$PORT" info memory 2>/dev/null | tr -d '\r' | awk -F: '/^used_memory:/{print $2}')
    keys=$(redis-cli -p "$PORT" dbsize | awk '{print $NF}')
    redis-cli -p "$PORT" flushall >/dev/null
    [ "${keys:-0}" -gt 0 ] || { echo 0; return; }
    echo $(( used / keys ))
}

BPE_16=$(measure 16)
BPE_1024=$(measure 1024)
echo "memgate: measured bytes/entry — 16B values: $BPE_16, 1024B values: $BPE_1024"

if [ "$MODE" = "--update-baseline" ]; then
    printf '{\n  "string_bpe_d16": %s,\n  "string_bpe_d1024": %s,\n  "band_pct": %s,\n  "recorded": "%s"\n}\n' \
        "$BPE_16" "$BPE_1024" "$BAND_PCT" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$BASELINE"
    echo "memgate: baseline recorded"
    exit 0
fi

[ -f "$BASELINE" ] || { echo "memgate: no baseline — run --update-baseline on lx64 first" >&2; exit 2; }
FAIL=0
for KEY in string_bpe_d16 string_bpe_d1024; do
    VAL=$([ "$KEY" = string_bpe_d16 ] && echo "$BPE_16" || echo "$BPE_1024")
    BASE=$(python3 -c "import json; print(json.load(open('$BASELINE'))['$KEY'])")
    OK=$(python3 -c "b=$BASE; v=$VAL; band=b*$BAND_PCT/100; print(1 if v <= b + band else 0)")
    if [ "$OK" != "1" ]; then
        echo "memgate: FAIL — $KEY measured $VAL > baseline $BASE +${BAND_PCT}%" >&2
        FAIL=1
    fi
done
[ "$FAIL" = "0" ] && echo "memgate: PASS"
exit "$FAIL"
