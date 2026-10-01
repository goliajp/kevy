#!/usr/bin/env bash
# perfgate — the perf tool: two kevy builds on one box, measured in one run.
#
#   bash bench/perfgate.sh compare A B        # per-angle ratio table, B / A
#   bash bench/perfgate.sh gate [CANDIDATE]   # CANDIDATE (default HEAD) against the
#                                             # reference, pass / fail / noisy
#   bash bench/perfgate.sh callgrind A B      # exact instructions per op, per function
#
# A build is a path to a kevy binary, or a git rev built on demand with the
# release-perf profile and cached under bench/.perfgate-ref/: v6.4.0, HEAD,
# a branch, HEAD+kevy-alloc (cargo features after the +), merge-base (where
# HEAD left origin/develop) or last-release. The gate's reference is
# `reference` in bench/perfgate.json (merge-base) unless --ref names one.
# A_ENV / B_ENV add environment to one side, so one binary can be compared
# with itself: A_ENV=KEVY_IO_URING=0 B_ENV=KEVY_IO_URING=1 compares reactors.
#
# Options: --rounds N (5), --angles "pinned_get pinned_set ..." (all),
# --windows N and --secs S per side (2 x 3 s), --ops N for callgrind (200000).
#
# What it measures. Each angle is a workload shape (perfgate_angles.py). In
# every round, each side gets a fresh server pinned to the box's server cpus,
# its keys, its load on the load cpus, and windows in which the tool reads
#   ops/s        the server's own command counter over a timed window
#   instr/op     instructions per command, user and kernel (perf stat -p)
#   cyc/op       cycles per command
#   sys/op       syscalls per command
#   util         server cpu time / (window x server cpus)
#   fgn%         cpu the rest of the box used during the window
# The side that goes first alternates by round and angle. Every metric is
# reported as the median of its per-round ratios B / A with the half-range
# of those ratios next to it, and the medians per side below that.
#
# Verdicts (the limits in bench/perfgate.json, on B / A): throughput
# at least 0.92, instructions per op at most 1.03, cycles per op at most
# 1.05, hybrid p95 at most 1.20. A metric passes when every round is inside
# its limit and fails when every round is beyond it; rounds on both sides of
# the limit are judged on their median if their spread is inside the noise
# bound, and are NOISY otherwise. Noisy means run it again. `gate` exits
# 0 pass, 1 fail, 3 noisy, 2 when it could not measure; `compare` prints the
# same verdicts and exits 0.
#
# The only checks before measuring: the box must be at least 95% idle
# (PERFGATE_IDLE_MIN=0 measures a busy box anyway, for a correctness run whose
# numbers nobody quotes), and
# the machine's bench lock is held for the whole run (builds happen before
# it is taken). After the run it names the other processes that used CPU.
# Topology per box is in bench/perfgate.json, picked by hostname or
# PERFGATE_BOX. Counters: perf stat as root; otherwise the bench account's
# /usr/local/sbin/kevy-perfstat helper through sudo.
#
# callgrind runs one shard under valgrind for a fixed request count per
# angle (get set incr sadd hset lpush zadd; default get set) and prints
# instructions per op for the whole server and per function, A against B.
# It does not depend on the box's load, so it is the first answer to "did
# instructions regress"; it cannot see cycles or the kernel.
set -eu
[ $# -gt 0 ] || { sed -n '2,17p' "$0"; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd)
# builds what is missing; a second pass, after the lock, finds it all built.
# Skipping it when a caller already holds the lock left the reference
# unbuilt, and the run died before measuring anything.
python3 "$HERE/perfgate.py" prepare --for "${1:-}" "${@:2}"
. "$HERE/bench-lock.sh"
exec python3 "$HERE/perfgate.py" "$@"
