#!/usr/bin/env bash
# measure-first -c50 bottleneck localization for kevy io_uring (the -c50-optimal
# reactor). Part A: is 4.4M client-bound? hold server=10sh, scale client cores.
# Part B: how does kevy scale with shards? hold client=6 cores, scale shards.
# All isolated, host-loopback, GET -c50 -P16 (cleanest read path). Reports
# steady-state overall rps.
set -u
KBIN=/root/kevy/target/release/kevy
N=${N:-4000000}

overall() { # port -> rps of last GET sample
  printf '%s' "$1" | tr '\r' '\n' | grep "^GET: rps=" | tail -1 \
    | sed -E 's/^[A-Z]+: rps=[0-9.]+ \(overall: ([0-9.]+)\).*/\1/'
}
wait_ready() {
  for _ in $(seq 1 100); do
    redis-benchmark -h 127.0.0.1 -p "$1" -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.1
  done; echo "!! $1 down"; return 1
}
start_kevy() { # srv_cores threads
  KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "$1" "$KBIN" --threads "$2" --port 7001 --no-aof >/tmp/pd.log 2>&1 &
  KPID=$!; wait_ready 7001
  redis-benchmark -h 127.0.0.1 -p 7001 -t set -n 200000 -P 16 -q >/dev/null 2>&1 # warm
}
stop_kevy() { kill "$KPID" 2>/dev/null; wait "$KPID" 2>/dev/null; }
measure() { # cli_cores cli_threads
  local raw
  raw=$(taskset -c "$1" redis-benchmark -h 127.0.0.1 -p 7001 -t get -n "$N" -c 50 -P 16 --threads "$2" -q 2>/dev/null)
  overall "$raw"
}

echo "### load: $(uptime)"
echo "=== Part A: client-core scan (server = io_uring 10sh on 0-9, fixed) ==="
start_kevy 0-9 10
for cfg in "10 1" "10-11 2" "10-13 4" "10-15 6"; do
  set -- $cfg
  r=$(measure "$1" "$2")
  printf '[A] client cores=%s threads=%s -> GET %s rps\n' "$1" "$2" "$r"
done
stop_kevy

echo "=== Part B: shard scan (client = 6 cores 10-15, fixed) ==="
for sc in "0-1 2" "0-3 4" "0-7 8" "0-9 10"; do
  set -- $sc
  start_kevy "$1" "$2"
  r=$(measure 10-15 6)
  printf '[B] shards=%s (cores=%s) -> GET %s rps\n' "$2" "$1" "$r"
  stop_kevy
done
echo "### DIAG_DONE"
