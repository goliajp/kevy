#!/usr/bin/env bash
# How many writes ride one fsync? — the number that decides the write design.
#
#   sudo bash bench/fsyncprobe.sh [rows] [conc]
#
# At matched durability (`appendfsync = always`) kevy does 10,455 writes/s at
# 64 clients against PostgreSQL's 30,145. Two explanations fit that equally
# well and lead to opposite designs:
#
#   * batching is not happening — each write pays its own fsync, and the fix
#     is a commit window;
#   * batching is happening and the device is the floor — and then a window
#     buys nothing, because the fsyncs are already as few as they can be.
#
# Guessing between them is exactly the hand-wave the perf methodology bans,
# so this counts the syscalls. `perf` counts a kernel tracepoint rather than
# ptrace-ing the server, so the server is not stopped on every syscall the
# way strace would stop it.
#
# Reported per engine: writes/s, fdatasync/s, and writes-per-fsync. Plus the
# device's own ceiling from a bare fdatasync loop on the same filesystem —
# an independent witness, so "the device is the floor" is a measured claim
# and not the shape of the data.
#
# Needs root for perf (perf_event_paranoid=3 on this box). The server itself
# still runs as the bench account.
set -uo pipefail

ROWS=${1:-500000}
CONC=${2:-64}
HERE="$(cd "$(dirname "$0")/.." && pwd)"
BENCH_USER=${PGCMP_USER:-kevybench}
BENCH_HOME=$(getent passwd "$BENCH_USER" | cut -d: -f6)
VENV=${PGCMP_VENV:-$BENCH_HOME/pgbench-venv/bin/python}
KEVY=${KEVY_BIN:-$HERE/target/release/kevy}
KPORT=${PGCMP_KPORT:-6392}
PGPORT=${PGCMP_PGPORT:-15499}
WORK=${PGCMP_WORK:-$BENCH_HOME/pgcmp-fsync}
OPS=${PGCONC_OPS:-600}
DUR=${FSYNC_DUR:-60}

refuse() { echo "fsyncprobe: REFUSED — $1" >&2; exit 2; }
[ "$(id -u)" = "0" ] || refuse "needs root for perf (perf_event_paranoid=3)"
[ -x "$KEVY" ] || refuse "no kevy binary at $KEVY"
command -v perf >/dev/null || refuse "no perf"

asuser() { sudo -u "$BENCH_USER" "$@"; }
SRVPAT="--port $KPORT --dir $WORK"

cleanup() { pkill -f -- "$SRVPAT" 2>/dev/null; }
trap cleanup EXIT

mkdir -p "$WORK"; chown "$BENCH_USER:$BENCH_USER" "$WORK"
CSV="$WORK/data.csv"

echo "== the device's own ceiling =="
# The independent witness. If kevy's fdatasync/s sits at this number then the
# device is the floor and no amount of batching design moves it; if it sits
# far below, the fsyncs are not the constraint.
asuser "$VENV" - "$WORK" <<'PY'
import os, sys, time
p = os.path.join(sys.argv[1], "fsync-ceiling.bin")
f = os.open(p, os.O_CREAT | os.O_WRONLY, 0o644)
os.write(f, b"x" * 4096); os.fsync(f)
n, t0 = 0, time.perf_counter()
while time.perf_counter() - t0 < 3.0:
    os.pwrite(f, b"y" * 4096, 0)
    os.fdatasync(f)
    n += 1
el = time.perf_counter() - t0
os.close(f); os.unlink(p)
print(f"  bare fdatasync: {n/el:,.0f}/s  ({el/n*1e6:,.0f} us each)")
PY

echo "== dataset =="
asuser "$VENV" "$HERE/bench/pgcompare.py" gen --rows "$ROWS" --out "$CSV" --pad 400

probe() { # $1 = label, $2 = pid list (comma), $3.. = the load command
  local label=$1 pids=$2; shift 2
  local log="$WORK/$label.perf"
  perf stat -e syscalls:sys_enter_fdatasync,syscalls:sys_enter_fsync \
    -p "$pids" -o "$log" -- "$@" > "$WORK/$label.out" 2>"$WORK/$label.err"
  local secs
  secs=$(awk '/seconds time elapsed/{print $1}' "$log")
  local fd fs
  fd=$(awk '/sys_enter_fdatasync/{gsub(",","",$1); print $1}' "$log")
  fs=$(awk '/sys_enter_fsync/{gsub(",","",$1); print $1}' "$log")
  echo "  $label: fdatasync=$fd fsync=$fs over ${secs}s"
  asuser "$VENV" - "$WORK/$label.out" "${fd:-0}" "${fs:-0}" "${secs:-1}" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
syncs = int(sys.argv[2]) + int(sys.argv[3]); secs = float(sys.argv[4])
for r in rows:
    w = r.get("write_ops_per_s")
    if not w:
        continue
    print(f"    conc={r['conc']:<4} writes/s={w:>10,}  fsync/s={syncs/secs:>10,.0f}"
          f"  writes per fsync={w/(syncs/secs) if syncs else float('inf'):>8.2f}")
PY
}

echo "== kevy, appendfsync=always =="
DIR="$WORK/kevy-always"; rm -rf "$DIR"; mkdir -p "$DIR"; chown "$BENCH_USER:$BENCH_USER" "$DIR"
cat > "$DIR/kevy.toml" <<TOML
[server]
data_dir = "$DIR"

[persistence]
aof         = true
appendfsync = "always"
TOML
chown "$BENCH_USER:$BENCH_USER" "$DIR/kevy.toml"
KEVY_BIND=127.0.0.1 asuser "$KEVY" --port "$KPORT" --dir "$DIR" --threads 8 \
  --config "$DIR/kevy.toml" >"$DIR/srv.log" 2>&1 &
for _ in $(seq 120); do
  asuser "$VENV" - "$KPORT" <<'PY' >/dev/null 2>&1 && break
import socket,sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1.0)
s.settimeout(2.0); s.sendall(b"PING\r\n")
sys.exit(0 if s.recv(64).startswith(b"+PONG") else 1)
PY
  sleep 0.5
done
EFF=$(redis-cli -p "$KPORT" CONFIG GET appendfsync 2>/dev/null | tail -1)
[ "$EFF" = "always" ] || refuse "appendfsync is '$EFF' — the probe would count the wrong policy"
asuser "$VENV" "$HERE/bench/pgcompare.py" kevy --csv "$CSV" --port "$KPORT" \
  --mode always --datadir "$DIR" --samples 1 >/dev/null || refuse "the kevy load failed"
KPIDS=$(pgrep -d, -f -- "$SRVPAT")
[ -n "$KPIDS" ] || refuse "no kevy pid to attach to"
probe kevy "$KPIDS" asuser "$VENV" "$HERE/bench/pgconc.py" kevy --port "$KPORT" \
  --rows "$ROWS" --conc "$CONC" --ops "$OPS" --shapes write --mode always
pkill -f -- "$SRVPAT" 2>/dev/null

echo "== postgres 18 =="
PGPIDS=$(pgrep -d, -f "cluster_name=${PGCMP_CLUSTER:-kevypgcmp}")
[ -n "$PGPIDS" ] || refuse "no postgres process carrying the cluster marker"
DSN="host=127.0.0.1 port=$PGPORT user=postgres password=bench dbname=bench"
asuser "$VENV" "$HERE/bench/pgcompare.py" pg --csv "$CSV" \
  --cluster "${PGCMP_CLUSTER:-kevypgcmp}" --dsn "$DSN" --samples 1 >/dev/null \
  || refuse "the postgres load failed"
# Re-read the pid list: the load may have forked backends the earlier list missed.
PGPIDS=$(pgrep -d, -f "cluster_name=${PGCMP_CLUSTER:-kevypgcmp}")
probe postgres "$PGPIDS" asuser "$VENV" "$HERE/bench/pgconc.py" pg --dsn "$DSN" \
  --rows "$ROWS" --conc "$CONC" --ops "$OPS" --shapes write

echo
echo "fsyncprobe: done — see $WORK/*.perf for the raw counts"
