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
# --dir, always: without it the server's data directory is the CURRENT
# directory, and this gate ran from the repo root — the aof-0.aof +
# shards.meta pair that kept appearing there between suite rounds was
# this line's single shard.
"$BIN" --port "$PORT" --threads 1 --dir "$DIR/data" &> "$DIR/server.log" &
SRV=$!
trap 'kill $SRV 2>/dev/null; sleep 0.2; kill -9 $SRV 2>/dev/null; wait $SRV 2>/dev/null; rm -rf "$DIR"' EXIT
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

# ── B7 (T5): cold-key formula — stub bytes/entry ≈ ENTRY_OVERHEAD(96) +
# key heap bytes, ±BAND_PCT (RFC 2026-07-24-v5-capacity-arc §2 B7).
# Analytic formula, no baseline entry: measured = stub_bytes/cold_keys
# from the new `INFO # Tiering` gauges on a second, tiered server
# instance (small budget so the benchmark's working set demotes).
# redis-benchmark keys ("key:____rand____", 16 B) sit under the 22-byte
# inline boundary → key heap = 0 → formula = 96 exactly.
b7_cold_bpe() {
    local port=$((PORT + 1)) formula=96
    local tdir; tdir=$(mktemp -d)
    KEVY_TIER_BUDGET=8mb "$BIN" --port "$port" --threads 1 --dir "$tdir" \
        &> "$DIR/server-tier.log" &
    local srv=$!
    # shellcheck disable=SC2064  # expand $srv/$tdir now, not at trap time
    trap "kill $srv 2>/dev/null; sleep 0.2; kill -9 $srv 2>/dev/null; wait $srv 2>/dev/null; rm -rf '$tdir'" RETURN
    sleep 1
    redis-benchmark -p "$port" -t set -n 20000 -r 20000 -d 4096 -q >/dev/null 2>&1
    sleep 1  # let the tick continuation drain the spill backlog
    local cold stub
    cold=$(redis-cli -p "$port" info tiering 2>/dev/null | tr -d '\r' | awk -F: '/^cold_keys:/{print $2}')
    stub=$(redis-cli -p "$port" info tiering 2>/dev/null | tr -d '\r' | awk -F: '/^stub_bytes:/{print $2}')
    if [ -z "${cold:-}" ] || [ "${cold:-0}" -eq 0 ]; then
        echo "memgate: B7 FAIL — tiered instance demoted nothing (cold_keys=${cold:-absent})" >&2
        return 1
    fi
    local bpe=$(( stub / cold ))
    local lo=$(( formula * (100 - BAND_PCT) / 100 )) hi=$(( formula * (100 + BAND_PCT) / 100 ))
    echo "memgate: B7 (T5) cold-key bytes/entry: $bpe (cold_keys=$cold) vs formula $formula ±${BAND_PCT}%"
    if [ "$bpe" -lt "$lo" ] || [ "$bpe" -gt "$hi" ]; then
        echo "memgate: B7 FAIL — cold bytes/entry $bpe outside [$lo, $hi]" >&2
        return 1
    fi
    return 0
}
B7_FAIL=0
b7_cold_bpe || B7_FAIL=1

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
FAIL=$(( FAIL | B7_FAIL ))
[ "$FAIL" = "0" ] && echo "memgate: PASS"
exit "$FAIL"
