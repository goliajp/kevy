#!/usr/bin/env bash
# v0.metal-1 step 1: expose the MEMORY WALL the single-key bench hides. Preload N
# keys, then GET random keys across the whole keyspace (-r N) so lookups miss
# L2/L3 and hit DRAM as N grows. Single shard on core 0, client on cores 1-15
# (not the bottleneck), io_uring, host-loopback. Throughput should fall as N
# passes cache — that's the real-workload ceiling.
set -u
K=${KBIN:-/root/kevy/target/release/kevy}
ready(){ i=0; while [ $i -lt 100 ]; do redis-benchmark -p "$1" -t ping -c1 -n1 -q >/dev/null 2>&1 && return; sleep 0.1; i=$((i+1)); done; }
ov(){ printf '%s' "$1" | tr '\r' '\n' | grep "^$2: rps=" | tail -1 | sed -E 's/.*rps=[0-9.]+ \(overall: ([0-9.]+)\).*/\1/'; }

echo "### metal large-keyspace (single shard io_uring, GET -c50 -P256, client 1-15)"
KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 "$K" --threads 1 --port 7001 --no-aof >/tmp/mk.log 2>&1 &
P=$!
ready 7001 || { echo down; kill $P; exit 1; }
for N in 1 100000 1000000 10000000; do
  # populate ~N keys (random, -r N) so the working set is N keys
  taskset -c 1-15 redis-benchmark -h 127.0.0.1 -p 7001 -t set -r "$N" -n "$N" -P 32 --threads 15 -q >/dev/null 2>&1
  raw=$(taskset -c 1-15 redis-benchmark -h 127.0.0.1 -p 7001 -t get -r "$N" -n 10000000 -c 50 -P 256 --threads 15 -q 2>/dev/null)
  printf '  keyspace=%-9s GET overall=%s rps\n' "$N" "$(ov "$raw" GET)"
done
kill "$P" 2>/dev/null; wait "$P" 2>/dev/null
echo "### MK_DONE"
