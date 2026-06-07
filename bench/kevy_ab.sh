#!/usr/bin/env bash
# A/B one kevy binary at -c50 -P16 (epoll + io_uring), isolated on cores 0-9,
# client on disjoint cores 10-15. Reports STEADY-STATE "overall" rps (the final
# "requests per second" line is quantized under --threads and unreliable).
# Args: <binary-path> <label>.
set -u
KBIN=${1:?binary path}
LABEL=${2:?label}
SRV_CORES=${SRV_CORES:-0-9}
CLI_CORES=${CLI_CORES:-10-15}
CLI_THREADS=${CLI_THREADS:-6}
N=${N:-4000000}
KEVY_THREADS=${KEVY_THREADS:-10}

wait_ready() {
  for _ in $(seq 1 100); do
    redis-benchmark -h 127.0.0.1 -p "$1" -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.1
  done; echo "!! port $1 down"; return 1
}
# steady-state overall rps = last "overall:" sample of each op
bench() { # port label
  local raw t v
  raw=$(taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p "$1" -t get,set \
    -n "$N" -c 50 -P 16 --threads "$CLI_THREADS" -q 2>/dev/null)
  for t in GET SET; do
    v=$(printf '%s' "$raw" | tr '\r' '\n' | grep "^$t: rps=" \
      | tail -1 | sed -E 's/^[A-Z]+: rps=[0-9.]+ \(overall: ([0-9.]+)\).*/\1/')
    printf '[%s] %s overall=%s\n' "$2" "$t" "$v"
  done
}
run() { # port label
  wait_ready "$1" || return
  redis-benchmark -h 127.0.0.1 -p "$1" -t set -n 200000 -P 16 -q >/dev/null 2>&1
  bench "$1" "$2"; bench "$1" "$2"
}

echo "### $LABEL  ($KBIN)  -c50 -P16 n=$N  threads=$KEVY_THREADS"
echo "--- $LABEL epoll ---"
KEVY_IO_URING=0 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$KEVY_THREADS" --port 7001 --no-aof >/tmp/ab_srv.log 2>&1 &
P=$!; run 7001 "$LABEL-epoll"; kill "$P" 2>/dev/null; wait "$P" 2>/dev/null

echo "--- $LABEL io_uring ---"
KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$KEVY_THREADS" --port 7001 --no-aof >/tmp/ab_uring.log 2>&1 &
P=$!; run 7001 "$LABEL-uring"; kill "$P" 2>/dev/null; wait "$P" 2>/dev/null
echo "### AB_DONE $LABEL"
