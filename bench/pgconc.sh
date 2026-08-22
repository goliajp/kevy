#!/usr/bin/env bash
# The concurrency sweep: kevy and PostgreSQL 18 under 1 / 8 / 32 / 64 clients.
#
#   bash bench/pgconc.sh [rows] [pad-bytes]
#
# WHAT THIS ADDS TO pgcompare.sh. That script drives one connection and so
# reports latency with nothing else in flight. This one holds the shapes,
# the rows and the harness fixed and moves only the client count, which is
# the axis an RDS is actually chosen on.
#
# CORES ARE SPLIT, AND SPLIT EQUALLY. The box has 16. The engine under test
# gets 0-7 — kevy via taskset with a matching --threads 8, PostgreSQL via
# the container's cpuset — and the driver gets 8-15. Without this the 64
# client processes and a 16-shard busy-poll server fight over the same
# cores and the sweep measures the scheduler. Each engine sees exactly the
# same eight cores, so the comparison stays symmetric.
#
# Expects the same Postgres container pgcompare.sh does, on PGCMP_PGPORT.
set -uo pipefail

ROWS=${1:-2000000}
PAD=${2:-400}
HERE="$(cd "$(dirname "$0")/.." && pwd)"
PY="$HERE/bench/pgcompare.py"
CONC_PY="$HERE/bench/pgconc.py"
VENV=${PGCMP_VENV:-$HOME/pgbench-venv/bin/python}
KEVY=${KEVY_BIN:-$HERE/target/release/kevy}
PGPORT=${PGCMP_PGPORT:-15499}
KPORT=${PGCMP_KPORT:-6391}
WORK=${PGCMP_WORK:-$HOME/pgcmp-conc}
OUT="$WORK/conc.jsonl"
CONC=${PGCONC_LEVELS:-1,8,32,64}
OPS=${PGCONC_OPS:-400}
SRV_CPUS=${PGCONC_SRV_CPUS:-0-7}
DRV_CPUS=${PGCONC_DRV_CPUS:-8-15}
SRV_THREADS=${PGCONC_SRV_THREADS:-8}
DRV_CORES=${PGCONC_DRV_CORES:-8}

refuse() { echo "pgconc: REFUSED — $1" >&2; exit 2; }
[ -x "$KEVY" ] || refuse "no kevy binary at $KEVY"
[ -x "$VENV" ] || refuse "no venv python at $VENV (needs psycopg)"
command -v taskset >/dev/null || refuse "no taskset — the core split is the fairness of this run"

mkdir -p "$WORK"; : >"$OUT"
CSV="$WORK/data.csv"

cleanup() {
  [ -n "${KPORT}" ] && pkill -f "kevy --port $KPORT" 2>/dev/null
  "$VENV" -c "
import psycopg
with psycopg.connect('host=127.0.0.1 port=$PGPORT user=postgres password=bench dbname=bench') as c:
    c.execute('DROP TABLE IF EXISTS t')
" 2>/dev/null || true
}
trap cleanup EXIT

echo "== dataset =="
"$VENV" "$PY" gen --rows "$ROWS" --out "$CSV" --pad "$PAD"

echo "== postgres 18 (cpus $SRV_CPUS) =="
"$VENV" -c "import socket,sys; socket.create_connection(('127.0.0.1', $PGPORT), 3).close()" \
  || refuse "no Postgres on 127.0.0.1:$PGPORT (see bench/pgcompare.sh for the runbook)"
# --samples 1 because this call is here to LOAD; the sweep does the timing.
"$VENV" "$PY" pg --csv "$CSV" --cluster "${PGCMP_CLUSTER:-kevypgcmp}" \
  --dsn "host=127.0.0.1 port=$PGPORT user=postgres password=bench dbname=bench" \
  --samples 1 >"$WORK/pg-load.json" || refuse "the postgres load failed"
taskset -c "$DRV_CPUS" "$VENV" "$CONC_PY" pg \
  --dsn "host=127.0.0.1 port=$PGPORT user=postgres password=bench dbname=bench" \
  --rows "$ROWS" --conc "$CONC" --ops "$OPS" --driver-cores "$DRV_CORES" \
  | tee -a "$OUT"

for MODE in everysec tiered; do
  echo "== kevy (aof=$MODE, cpus $SRV_CPUS, threads $SRV_THREADS) =="
  DIR="$WORK/kevy-$MODE"; rm -rf "$DIR"; mkdir -p "$DIR"
  {
    printf '[server]\ndata_dir = "%s"\n\n[persistence]\naof = true\n' "$DIR"
    printf 'appendfsync = "everysec"\n'
    [ "$MODE" = "tiered" ] && printf '\n[tiering]\nbudget = "%s"\n' "${PGCONC_TIER_BUDGET:-2gb}"
  } > "$DIR/kevy.toml"
  KEVY_BIND=127.0.0.1 taskset -c "$SRV_CPUS" "$KEVY" --port "$KPORT" --dir "$DIR" \
    --threads "$SRV_THREADS" --config "$DIR/kevy.toml" >"$DIR/srv.log" 2>&1 &
  for _ in $(seq 120); do
    "$VENV" - "$KPORT" <<'PY' >/dev/null 2>&1 && break
import socket,sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1.0)
s.settimeout(2.0); s.sendall(b"PING\r\n")
sys.exit(0 if s.recv(64).startswith(b"+PONG") else 1)
PY
    sleep 0.5
  done
  T=$(redis-cli -p "$KPORT" INFO server 2>/dev/null | tr -d '\r' | awk -F: '/^kevy_version:/{print $2}')
  [ -n "$T" ] || refuse "kevy on $KPORT never answered — the level would be mislabelled"
  echo "  kevy $T up on cpus $SRV_CPUS"
  "$VENV" "$PY" kevy --csv "$CSV" --port "$KPORT" --mode "$MODE" \
    --datadir "$DIR" --samples 1 >"$WORK/kevy-$MODE-load.json" || refuse "the kevy load failed"
  taskset -c "$DRV_CPUS" "$VENV" "$CONC_PY" kevy --port "$KPORT" \
    --rows "$ROWS" --conc "$CONC" --ops "$OPS" --mode "$MODE" \
    --label "kevy$T" --driver-cores "$DRV_CORES" | tee -a "$OUT"
  pkill -f "kevy --port $KPORT" 2>/dev/null
  for _ in $(seq 60); do pgrep -f "kevy --port $KPORT" >/dev/null 2>&1 || break; sleep 0.5; done
done

echo
echo "== results ($OUT) =="
"$VENV" - "$OUT" <<'PY'
import json, sys
SHAPES = ("pk", "idx", "page", "write")
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
if not rows:
    sys.exit("no results")
head = "  " + "engine/mode".ljust(22) + "conc".rjust(5)
for s in SHAPES:
    head += (s + " p50/p99/ops").rjust(26)
print(head + "drv".rjust(7))
for r in rows:
    line = "  " + (r["engine"] + "/" + r["mode"]).ljust(22) + str(r["conc"]).rjust(5)
    for s in SHAPES:
        cell = "{}/{}/{:,}".format(r[s + "_p50_us"], r[s + "_p99_us"], r[s + "_ops_per_s"])
        line += cell.rjust(26)
    line += str(r["driver_cores_max"]).rjust(7)
    if r["driver_saturated"]:
        line += "  DRIVER-SATURATED"
    print(line)
PY
