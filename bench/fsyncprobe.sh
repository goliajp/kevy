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

# kevy runs the io_uring reactor here, and IORING_OP_FSYNC never becomes an
# fdatasync SYSCALL — so counting syscalls alone reported kevy as doing 5
# fsyncs while it served 7,250 writes/s, a number that looked like batching
# and was blindness. Both interfaces are counted; this is the opcode.
OP_FSYNC=3

# fsync on tmpfs is free, and a data directory under /tmp makes every
# durability number in this probe a fiction. (It made one: a validation run
# put the store in mktemp -d and measured 34,500 fsync/s on a device that
# does 1,100.)
fstype() { stat -f -c %T "$1" 2>/dev/null; }

asuser() { sudo -u "$BENCH_USER" "$@"; }
SRVPAT="--port $KPORT --dir $WORK"

cleanup() { pkill -f -- "$SRVPAT" 2>/dev/null; }
trap cleanup EXIT

mkdir -p "$WORK"; chown "$BENCH_USER:$BENCH_USER" "$WORK"
FS=$(fstype "$WORK")
case "$FS" in tmpfs|ramfs) refuse "$WORK is $FS — fsync there is free and every number below would be a fiction" ;; esac
echo "  store filesystem: $FS"
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

# System-wide, not per-pid. PostgreSQL forks a backend per connection, so
# `perf -p` would attach to the postmaster and miss every backend the sweep
# creates — the fsyncs would be counted as zero and read as "no fsyncs".
# System-wide catches whatever forks, at the cost of catching the box's other
# containers too, which is what the idle baseline below is for.
idle_syncs() { # $1 = seconds — the box's own fsync noise with nothing loaded
  local log="$WORK/idle.perf"
  perf stat -a -e syscalls:sys_enter_fdatasync -e syscalls:sys_enter_fsync \
    -e io_uring:io_uring_submit_req --filter "opcode==$OP_FSYNC" \
    -o "$log" -- sleep "$1" >/dev/null 2>&1
  awk '/sys_enter_f(data)?sync|io_uring_submit_req/{gsub(",","",$1); t+=$1} END{printf "%d", t}' "$log"
}

probe() { # $1 = label, $2.. = the load command
  local label=$1; shift
  local log="$WORK/$label.perf"
  perf stat -a -e syscalls:sys_enter_fdatasync -e syscalls:sys_enter_fsync \
    -e io_uring:io_uring_submit_req --filter "opcode==$OP_FSYNC" \
    -o "$log" -- "$@" > "$WORK/$label.out" 2>"$WORK/$label.err"
  local secs tot
  secs=$(awk '/seconds time elapsed/{print $1}' "$log")
  tot=$(awk '/sys_enter_f(data)?sync|io_uring_submit_req/{gsub(",","",$1); t+=$1} END{printf "%d", t}' "$log")
  # An empty counter formats as a blank in a sentence and reads like a
  # measurement. It is the absence of one: perf could not exec the workload,
  # or attached to nothing. Refuse rather than print a row with holes in it.
  [ -n "$secs" ] || { sed -n "1,20p" "$log" >&2; refuse "$label: perf produced no elapsed time — see $log and $WORK/$label.err"; }
  [ "${tot:-0}" -gt 0 ] || { tail -3 "$WORK/$label.err" >&2; refuse "$label: zero fsync syscalls over ${secs}s — the probe measured nothing"; }
  echo "  $label: fsync syscalls=$tot over ${secs}s (box idle noise: $IDLE_RATE/s)"
  sudo -u "$BENCH_USER" "$VENV" - "$WORK/$label.out" "$tot" "$secs" "$IDLE_RATE" <<'PYEOF'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
tot, secs, idle = int(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4])
rate = tot / secs - idle          # the box's other containers, subtracted out
for r in rows:
    w = r.get("write_ops_per_s")
    if not w:
        continue
    per = w / rate if rate > 0 else float("inf")
    print(f"    conc={r['conc']:<4} writes/s={w:>10,}  fsync/s={rate:>10,.0f}"
          f"  writes per fsync={per:>8.2f}")
PYEOF
}

echo "== the box's own fsync noise =="
IDLE_RATE=$(sudo -u "$BENCH_USER" "$VENV" -c "print($(idle_syncs 10)/10.0)")
echo "  idle: $IDLE_RATE fsync/s from everything else on this box"

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
probe kevy sudo -u "$BENCH_USER" "$VENV" "$HERE/bench/pgconc.py" kevy --port "$KPORT" \
  --rows "$ROWS" --conc "$CONC" --ops "$OPS" --shapes write --mode always
pkill -f -- "$SRVPAT" 2>/dev/null

echo "== postgres 18 =="
DSN="host=127.0.0.1 port=$PGPORT user=postgres password=bench dbname=bench"
asuser "$VENV" "$HERE/bench/pgcompare.py" pg --csv "$CSV" \
  --cluster "${PGCMP_CLUSTER:-kevypgcmp}" --dsn "$DSN" --samples 1 >/dev/null \
  || refuse "the postgres load failed"
probe postgres sudo -u "$BENCH_USER" "$VENV" "$HERE/bench/pgconc.py" pg --dsn "$DSN" \
  --rows "$ROWS" --conc "$CONC" --ops "$OPS" --shapes write

echo
echo "fsyncprobe: done — see $WORK/*.perf for the raw counts"
