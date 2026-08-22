#!/usr/bin/env bash
# kevy 4 vs PostgreSQL 18 (stock config) on one box, one dataset.
#
# Answers a question the repo could not answer before: kevy has been
# measured against valkey and against embedded KV engines, never against
# a relational database — so any "faster than an RDS" line was inference,
# not data. This produces the data.
#
#   bash bench/pgcompare.sh [rows] [pad-bytes]
#
# Defaults to 2,000,000 rows x ~440 B = ~1 GB of CSV.
#
# The comparison, stated so it can be checked:
#   * PostgreSQL 18 runs STOCK — shared_buffers=128MB, fsync=on,
#     synchronous_commit=on. That is what was asked for, and it is
#     conservative: a tuned PG would do better on every read line here.
#   * kevy runs in all three durability modes, because its stock config
#     has AOF OFF and pitting "no durability" against "fsync per commit"
#     would be a category error. Compare the row that matches the
#     durability you would actually run.
#   * Both are driven by the SAME Python harness, so client cost lands on
#     both sides and compresses the ratio — reported gaps are lower bounds.
#
# It expects a Postgres of its own already listening on PGCMP_PGPORT
# (see the runbook comment further down) and never touches a Postgres it
# was not pointed at — this box hosts other people's.
set -uo pipefail

ROWS=${1:-2000000}
PAD=${2:-400}
HERE="$(cd "$(dirname "$0")/.." && pwd)"
PY="$HERE/bench/pgcompare.py"
VENV=${PGCMP_VENV:-$HOME/pgbench-venv/bin/python}
KEVY=${KEVY_BIN:-$HERE/target/release/kevy}
PGPORT=${PGCMP_PGPORT:-15499}
KPORT=${PGCMP_KPORT:-6390}
CTR=kevy-pgcmp
WORK=${PGCMP_WORK:-$HOME/pgcmp}
OUT="$WORK/results.jsonl"

refuse() { echo "pgcompare: REFUSED — $1" >&2; exit 2; }
[ -x "$KEVY" ] || refuse "no kevy binary at $KEVY (cargo build --release -p kevy)"
[ -x "$VENV" ] || refuse "no venv python at $VENV (needs psycopg)"


mkdir -p "$WORK"; : >"$OUT"
CSV="$WORK/data.csv"

# Match the server by its flags, not by the binary's name: KEVY_BIN points at
# `kevy-off` / `kevy-on` for the allocator comparison, and "kevy --port N"
# matches neither — so nothing was ever killed between modes, every mode's
# server stayed bound to the same port under SO_REUSEPORT, and CONFIG GET
# reached whichever answered. Both guards below need a non-empty port.
SRVPAT="--port $KPORT --dir $WORK"

cleanup() {
  # killgate: an empty SRVPAT would make this pattern match every process
  # on the box — that took lx64 offline three times in 2026-07.
  [ -n "${SRVPAT}" ] || return 0
  pkill -f -- "$SRVPAT" 2>/dev/null
  # Drop the dataset too. A run left a 979 MB table loaded in the
  # always-on container, and postgres's autovacuum ground through it in
  # the background for hours — diskgate's rewrite measurement on the
  # same disk went from 42 ms to 553 ms on exactly one tier run, and
  # the spike was this container's housekeeping, not the engine. The
  # data is regenerated every run; keeping it buys nothing.
  "$VENV" -c "
import psycopg
with psycopg.connect('host=127.0.0.1 port=$PGPORT user=postgres password=bench dbname=bench') as c:
    c.execute('DROP TABLE IF EXISTS t')
" 2>/dev/null || true
}
trap cleanup EXIT

echo "== dataset =="
"$VENV" "$PY" gen --rows "$ROWS" --out "$CSV" --pad "$PAD"

echo "== postgres 18 (stock config) =="
# The container is expected to be running already: this box's bench
# account has no docker socket, and handing it one would make that
# account root-equivalent on a machine other people share. Start it as
# root, once:
#   docker run -d --name kevy-pgcmp -p 127.0.0.1:15499:5432 \
#     -e POSTGRES_PASSWORD=bench -e POSTGRES_DB=bench \
#     postgres:18.4-bookworm -c cluster_name=kevypgcmp
# cluster_name is what lets the harness find this cluster's processes in
# /proc without docker, and keeps it off the other Postgres instances
# this box runs.
"$VENV" -c "import socket,sys; socket.create_connection(('127.0.0.1', $PGPORT), 3).close()" \
  || refuse "no Postgres on 127.0.0.1:$PGPORT (see the comment above this line)"
"$VENV" "$PY" pg --csv "$CSV" --cluster "${PGCMP_CLUSTER:-kevypgcmp}" \
  --dsn "host=127.0.0.1 port=$PGPORT user=postgres password=bench dbname=bench" \
  --samples "${PGCMP_SAMPLES:-5000}" | tee -a "$OUT"

for MODE in none everysec always tiered; do
  echo "== kevy 4 (aof=$MODE) =="
  DIR="$WORK/kevy-$MODE"; rm -rf "$DIR"; mkdir -p "$DIR"
  # appendfsync lives in the TOML only — there is no CLI or env face for
  # it, and passing `--appendfsync always` silently did nothing, which
  # made the "always" row a duplicate of "everysec" until this was
  # caught. Write a config per mode and assert the server agrees.
  case "$MODE" in
    none)
      ARGS="--no-aof" ;;
    tiered)
      # The memory column is the honest cost of holding rows in RAM, and
      # tiering is the answer to it: same durability as `everysec`, but a
      # RAM budget the store must stay inside while the rest of the data
      # lives on disk. Budget = a quarter of what the untiered run needed.
      cat > "$DIR/kevy.toml" <<TOML
[server]
data_dir = "$DIR"

[persistence]
aof          = true
appendfsync  = "everysec"

[tiering]
budget = "${PGCMP_TIER_BUDGET:-2gb}"
TOML
      ARGS="--config $DIR/kevy.toml" ;;
    *)
      cat > "$DIR/kevy.toml" <<TOML
[server]
data_dir = "$DIR"

[persistence]
aof          = true
appendfsync  = "$MODE"
TOML
      ARGS="--config $DIR/kevy.toml" ;;
  esac
  # shellcheck disable=SC2086
  KEVY_BIND=127.0.0.1 "$KEVY" --port "$KPORT" --dir "$DIR" $ARGS \
    >"$DIR/srv.log" 2>&1 &
  # Ready means it ANSWERS, not that the port accepts. The listener binds
  # before the AOF shards finish replaying, so a bare connect() succeeds on a
  # server that cannot yet serve a command — at 11.5 GB that gap was wide
  # enough for the CONFIG GET below to come back empty and refuse the run as
  # mislabelled, losing three of the four modes.
  for _ in $(seq 120); do
    "$VENV" - "$KPORT" <<'PY' >/dev/null 2>&1 && break
import socket,sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1.0)
s.settimeout(2.0); s.sendall(b"PING\r\n")
sys.exit(0 if s.recv(64).startswith(b"+PONG") else 1)
PY
    sleep 0.5
  done
  case "$MODE" in
    none) ;;
    tiered)
      B=$(redis-cli -p "$KPORT" INFO tiering 2>/dev/null | tr -d '\r' | awk -F: '/^tier_budget_bytes:/{print $2}')
      [ "${B:-0}" -gt 0 ] || refuse "tiering budget is '${B:-unset}' — the run would be mislabelled"
      echo "  tiering budget=$B bytes, appendfsync=$(redis-cli -p "$KPORT" CONFIG GET appendfsync 2>/dev/null | tail -1)" ;;
    *)
      EFF=$(redis-cli -p "$KPORT" CONFIG GET appendfsync 2>/dev/null | tail -1)
      [ "$EFF" = "$MODE" ] || refuse "appendfsync is '$EFF', asked for '$MODE' — the run would be mislabelled"
      echo "  appendfsync=$EFF (confirmed by CONFIG GET)" ;;
  esac
  "$VENV" "$PY" kevy --csv "$CSV" --port "$KPORT" --mode "$MODE" \
    --datadir "$DIR" --samples "${PGCMP_SAMPLES:-5000}" | tee -a "$OUT"
  # Wait for it to actually be gone rather than for a fixed second: freeing a
  # 17 GB resident set takes the kernel longer than that, and the next mode
  # then starts its own load under the dying process's memory pressure.
  if [ -n "${SRVPAT}" ]; then
    pkill -f -- "$SRVPAT" 2>/dev/null
    for _ in $(seq 60); do
      pgrep -f -- "$SRVPAT" >/dev/null 2>&1 || break
      sleep 0.5
    done
    sleep 1
    # SO_REUSEPORT lets a survivor keep answering on the same port, so a
    # failed teardown does not look like a failure — it looks like the next
    # mode's numbers. Prove the port is clear before opening the next one.
    pgrep -f -- "$SRVPAT" >/dev/null 2>&1 &&
      refuse "a server matching '$SRVPAT' outlived its mode; the next mode would share its port"
  fi
done

echo
echo "== results ($OUT) =="
"$VENV" - "$OUT" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
if not rows: sys.exit("no results")
hdr = ["engine/mode", "load MB/s", "pk p99", "idx p99", "page p99", "write p99",
       "disk KB/csvMB", "rss KB/csvMB"]
print("  " + " | ".join(f"{h:>13}" for h in hdr))
for r in rows:
    name = f"{r['engine']}/{r['mode']}"
    vals = [name, r.get("load_mb_per_s", "-"),
            f"{r.get('pk_p99_us','-')}us", f"{r.get('idx_p99_us','-')}us",
            f"{r.get('page_p99_us','-')}us", f"{r.get('write_p99_us','-')}us",
            r.get("disk_per_csv_mb_kb", "-"), r.get("rss_per_csv_mb_kb", "-")]
    print("  " + " | ".join(f"{str(v):>13}" for v in vals))
PY
