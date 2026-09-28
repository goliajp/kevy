# Sourced by perfgate.sh: hybrid-retrieval p95 (IDX.QUERY HYBRID, text MATCH
# fused with vector KNN), one client, closed loop.
#
# Its own topology, not the 8-thread one. lx64 is 8 cores x 2 SMT siblings
# (cpu i and i+8); the other angles put the server on 0-7 and the load on
# 8-15, so every client shares a physical core with some shard thread, and
# shard threads are not pinned. For throughput that averages out; for one
# client's latency it does not: the p95 split into two modes 80 µs apart,
# in phases of hundreds to thousands of requests, as shard threads drifted
# on and off the client's sibling. Four threads on 0-3 with the client alone
# on 5 leaves no core shared.
N_HYBRID=${N_HYBRID:-20000}
# Fresh servers per pass: the p95 moves by about 9% between instances of
# one binary (230-273 µs, one build, a quiet box), so the verdict needs
# more instances than the throughput angles and a wider band. Latency
# lines are judged at reference / LAT_TOL, about +20%: a real regression
# of that size is caught, instance scatter is not.
HYBRID_PER_PASS=${HYBRID_PER_PASS:-2}
LAT_TOL=${LAT_TOL:-0.83}

hybrid_server_start() {
  SRV_CPUS=0-3 SRV_THREADS=4 server_start ""
  python3 "$HERE/perfgate_hybrid.py" load 7001 >&2
}

# Prints the p95 in µs, or nothing when the angle broke (the gate refuses).
run_hybrid() {
  taskset -c 5 python3 "$HERE/perfgate_hybrid.py" run 7001 "$N_HYBRID"
}
