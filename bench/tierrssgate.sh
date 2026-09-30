#!/usr/bin/env bash
# tierrssgate — the tiering budget bounds the process: RSS <= budget x 1.05
# at every sample while a hash workload four times the budget loads, is read
# back cold, and is overwritten.
#
# The same row shape as capacity-envelope's D1 (five fields, a 900-byte
# one), scaled to run in a minute: 600,000 rows (~1.1 GB of rows) on a
# 256 MiB budget, 2 shards. At this scale the keyspace table doubles twice
# after the hot set has filled the budget, which is the case that leaves
# freed rows resident; and the fixed cost of the process (binary, receive
# rings, stacks) is a larger share of the budget than at full scale, so a
# budget that bounds only the store's own accounting fails here first.
#
#   bash bench/tierrssgate.sh            # builds target/release/kevy
#   KEVY_BIN=/path/to/kevy bash bench/tierrssgate.sh
#
# Knobs: TIERRSS_ROWS, TIERRSS_BUDGET (bytes), TIERRSS_PORT (default 6317).
# Needs Linux (RSS from /proc) and a data dir on real disk: set TMPDIR to
# one when /tmp is tmpfs. Exit 0 = held, 1 = RSS crossed the line or the
# workload failed, 2 = refused.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
PY="$HERE/capacity_envelope.py"
PORT=${TIERRSS_PORT:-6317}
ROWS=${TIERRSS_ROWS:-600000}
BUDGET=${TIERRSS_BUDGET:-$((256 * 1024 * 1024))}
LINE=$((BUDGET * 105 / 100))

refuse() { echo "tierrssgate: REFUSED — $1" >&2; exit 2; }
fail()   { echo "tierrssgate: FAIL — $1" >&2; exit 1; }

[ "$(id -u)" -ne 0 ] || refuse "refusing to run as root"
[ "$(uname)" = Linux ] || refuse "RSS is read from /proc — Linux only"
command -v python3 >/dev/null || refuse "python3 not installed"
DATA_BASE="${TMPDIR:-/tmp}"
case "$(stat -f -c %T "$DATA_BASE" 2>/dev/null)" in
  tmpfs|ramfs) refuse "$DATA_BASE is RAM-backed; tiering needs real disk (set TMPDIR)" ;;
esac

if [ -z "${KEVY_BIN:-}" ]; then
  ( cd "$HERE/.." && cargo build -q --release -p kevy --bin kevy ) || refuse "release build failed"
  KEVY_BIN="$HERE/../target/release/kevy"
fi
[ -x "$KEVY_BIN" ] || refuse "$KEVY_BIN is not executable"

DIR=$(mktemp -d "$DATA_BASE/tierrss-XXXXXX") && mkdir "$DIR/data"
SRV=""; SAMP=""
on_exit() {
  [ -n "$SAMP" ] && kill "$SAMP" 2>/dev/null
  [ -n "$SRV" ] && kill "$SRV" 2>/dev/null
  wait 2>/dev/null
  rm -rf "$DIR"
}
trap on_exit EXIT
kcmd()  { python3 "$PY" cmd --port "$PORT" -- "$@"; }
tinfo() { local v; v=$(python3 "$PY" info --port "$PORT" --field "$1"); echo "${v:-0}"; }

env KEVY_TIER_BUDGET="$BUDGET" KEVY_BIND=127.0.0.1 \
  "$KEVY_BIN" --port "$PORT" --threads 2 --dir "$DIR/data" --no-aof >"$DIR/srv.log" 2>&1 &
SRV=$!
for _ in $(seq 1 150); do
  [ "$(kcmd PING 2>/dev/null)" = "+PONG" ] && break
  kill -0 "$SRV" 2>/dev/null || break
  sleep 0.2
done
[ "$(kcmd PING 2>/dev/null)" = "+PONG" ] || refuse "server did not come up: $(tail -3 "$DIR/srv.log")"

# every 200 ms for the whole run: a line crossed between two samples of a
# slower sampler would pass unseen
( while kill -0 "$SRV" 2>/dev/null; do
    awk '/^VmRSS:/{print $2 * 1024}' "/proc/$SRV/status" 2>/dev/null >>"$DIR/rss"
    sleep 0.2
  done ) &
SAMP=$!

echo "tierrssgate: $ROWS rows x ~1 KiB on a $((BUDGET >> 20)) MiB budget (line $LINE)"
python3 "$PY" load-d1 --port "$PORT" --rows "$ROWS" --pad 900 --seed 1 || fail "load"
sleep 2
# read cold rows back (the second read of a row promotes it), then
# overwrite the oldest quarter: both move rows between the tiers
python3 "$PY" lat --port "$PORT" --n 20000 --rows "$ROWS" --seed 3 --mode coldrow >/dev/null \
  || fail "cold reads"
python3 "$PY" load-d1 --port "$PORT" --rows $((ROWS / 4)) --pad 900 --seed 2 || fail "overwrite"
sleep 3

COLD=$(tinfo cold_keys)
USED=$(python3 "$PY" info --port "$PORT" --field used_memory)
TRIMS=$(tinfo heap_trims_total); OVERHEAD=$(tinfo tier_overhead_bytes)
kill "$SAMP" 2>/dev/null; wait "$SAMP" 2>/dev/null; SAMP=""
N=$(wc -l <"$DIR/rss")
PEAK=$(sort -n "$DIR/rss" | tail -1)
LAST=$(tail -1 "$DIR/rss")
echo "tierrssgate: cold_keys=$COLD used_memory=$USED overhead=$OVERHEAD trims=$TRIMS"
echo "tierrssgate: RSS peak $PEAK ($(awk -v p="$PEAK" -v b="$BUDGET" 'BEGIN{printf "%.3f", p/b}')x budget) over $N samples, last $LAST"
[ "$COLD" -gt $((ROWS / 2)) ] || fail "demotion did not engage (cold_keys=$COLD)"
[ "$N" -ge 50 ] || fail "only $N RSS samples: the sampler did not run"
[ "${PEAK:-0}" -gt $((BUDGET / 2)) ] || fail "RSS peak $PEAK is below half the budget: the sampler read the wrong process"
[ "$PEAK" -le "$LINE" ] || fail "RSS peak $PEAK crossed budget x 1.05 = $LINE"
echo "tierrssgate: PASS — RSS held under budget x 1.05 through load, cold reads and overwrite"
