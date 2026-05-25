#!/usr/bin/env bash
# Pipeline scan to classify the server bottleneck WITHOUT perf: fixed server
# (io_uring 4sh on 0-3) + strong client (12 cores), vary -P. If low-P rps is far
# below high-P, per-command syscall/reactor overhead dominates (lever: io_uring
# multishot / syscall batching). If rps is flat across -P, per-command CPU
# dominates (lever: command-path CPU). host-loopback, GET.
set -u
KBIN=${1:-/root/kevy/target/release/kevy}
SRV_CORES=${SRV_CORES:-0-3}
SHARDS=${SHARDS:-4}
CLI_CORES=${CLI_CORES:-4-15}
CLI_THREADS=${CLI_THREADS:-12}

overall() { printf '%s' "$1" | tr '\r' '\n' | grep "^GET: rps=" | tail -1 \
  | sed -E 's/^[A-Z]+: rps=[0-9.]+ \(overall: ([0-9.]+)\).*/\1/'; }
wait_ready() { for _ in $(seq 1 100); do
  redis-benchmark -h 127.0.0.1 -p "$1" -t ping -c 1 -n 1 -q >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }

echo "### pipeline scan: io_uring ${SHARDS}sh on $SRV_CORES, client $CLI_CORES x$CLI_THREADS, GET -c50"
KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c "$SRV_CORES" "$KBIN" --threads "$SHARDS" --port 7001 --no-aof >/tmp/pp.log 2>&1 &
P=$!
wait_ready 7001 || { echo down; kill $P; exit 1; }
redis-benchmark -h 127.0.0.1 -p 7001 -t set -n 300000 -P 16 -q >/dev/null 2>&1
for pipe in 1 4 16 64 256; do
  n=4000000; [ "$pipe" -le 4 ] && n=1500000
  raw=$(taskset -c "$CLI_CORES" redis-benchmark -h 127.0.0.1 -p 7001 -t get \
    -n "$n" -c 50 -P "$pipe" --threads "$CLI_THREADS" -q 2>/dev/null)
  printf '  -P%-3s -> GET %s rps\n' "$pipe" "$(overall "$raw")"
done
kill "$P" 2>/dev/null; wait "$P" 2>/dev/null
echo "### PIPE_DONE"
