# kevy → metal: exhaustive perf/mem/size refactor (L1–L4 linear task set)

Authorized 2026-05-26: ignore ROI / win-risk. Try every metal-level lever; let
perf/mem/size data speak. **Correctness stays a hard gate** (sharded 11/11 epoll+
io_uring, full workspace tests, clippy 0 per checkpoint) — only the perf-WIN gate
is relaxed (try uncertain stones; keep what helps any of perf/mem/size, revert
only clear all-axis regressions; document every result in `perfs/`).

## L1 — Roadmap (one line)
Close the gap between kevy's current per-core (~169 ns/cmd ≈ ~590 cyc, ~3–4× above
the compute floor; single-shard 5.9M GET/core) and the **hardware ceilings** —
compute floor, memory latency, cross-core coherency, NIC zero-copy — measured on
lx64, optimizing perf + mem + size exhaustively.

## L2 — Version boundary `v0.metal` (locked; the work units / checkpoints)
Scope: pure perf/mem/size of the existing feature set. No new commands/features.
Charter intact (0 crates.io dep, pure-Rust, libc only in kevy-sys, no C). Ordered
linear checkpoints, each its own stone:

- **v0.metal-1 — Measurement foundation.** Large-keyspace bench (expose the memory
  wall the single-key bench hides), forced/▷profiled cross-core tax measurement,
  kevy-ring in-process micro-bench, perf-on-lx64 harness generalized, and RSS +
  binary-size tracking. Record all baselines.
- **v0.metal-2 — Zero-alloc local hot path.** Kill parse's 2 per-command allocs on
  the LOCAL path (borrow argv from the input buffer; owned only when forwarded).
  (reply alloc already removed by the in-order bypass.)
- **v0.metal-3 — Cross-core arena / message compaction.** Remove/pool the alloc on
  the cross-core forward path; shrink bytes + cache lines crossed per hop.
- **v0.metal-4 — Parse to the floor.** SWAR/SIMD CRLF + length scan; single-pass
  where possible; the profile's parse_command (~13.5%).
- **v0.metal-5 — Dispatch to the floor.** Branch-lean / perfect-hash verb dispatch
  replacing the `dispatch_* || …` chain; leaner routing.
- **v0.metal-6 — Memory wall.** Software prefetch of the next command's key bucket
  during batch processing; small-value/key inlining in the bucket (kill the
  pointer-chase cache miss); cache-conscious bucket layout.
- **v0.metal-7 — Pages & NUMA.** Hugepages (THP/explicit) for the store backing
  (TLB); NUMA-local shard memory (multi-socket; lx64 is single-socket → measure
  what applies).
- **v0.metal-8 — Zero-copy IO.** io_uring `SEND_ZC` for replies; registered files
  (fixed fd, skip the fd-table lookup) + registered buffers; revisit multishot
  tuning. (AF_XDP kernel-bypass = stretch, in-charter as a syscall iface; eval.)
- **v0.metal-9 — Footprint & size.** Per-conn + pbuf-ring + store memory overhead;
  binary size (LTO/codegen/opt-level/panic strategy) — the size axis.

## L3a — HOT plan (current checkpoint: v0.metal-1, fully linear)
Each step ends with a detection command; record results under
`perfs/data/2026-05-26/metal-*`.
1. **Large-keyspace bench** `bench/metal_keyspace.sh`: preload N keys, GET with
   `redis-benchmark -r N` (random keyspace) so lookups miss cache/DRAM. Single
   shard, N ∈ {1, 100k, 1M, 10M}. Detect: throughput falls as N grows past
   L2/L3 → the memory wall is now visible + baselined.
2. **kevy-ring in-process micro-bench** `crates/kevy-ring/examples/bench_ring.rs`:
   raw SPSC push/pop ns + cross-thread round-trip. Detect: ns/op recorded (the
   cross-core primitive floor).
3. **Cross-core tax** via a multi-shard `perf` profile (cross-core functions'
   self-%) + the ring micro-bench. Detect: cross-core CPU share recorded.
4. **perf/mem/size harness**: generalize `/tmp/perf_profile.sh` to take the bench
   cmd; capture server RSS (`/proc/<pid>/status` VmRSS) under load + `ls -l` +
   `size` of the release binary. Detect: `metal-baseline.txt` written with
   per-core ns, large-keyspace rps curve, ring ns, cross-core %, RSS, binary size.

## L3b — COLD plan (v0.metal-2 … v0.metal-9)
As listed in L2 — what / requirement / resource, NOT step-level (detail when
promoted to hot). Resource notes: all measurable on lx64 (perf installed) except
multi-core aggregate peak (client-bound — irrelevant to these internal stones) &
real-NIC zero-copy throughput (lx64 NIC = 100Mbit; SEND_ZC correctness still
verifiable, throughput not). Large-keyspace + cross-core + per-core are all
lx64-measurable.

## L4 — Triggers (cold → hot promotion predicates)
- 1→2: `metal-baseline.txt` exists with all five baselines (per-core ns,
  large-keyspace curve, ring ns, cross-core %, RSS+size) + harnesses re-runnable.
- 2→3 … 8→9: previous checkpoint merged to develop with sharded 11/11 (epoll+
  io_uring) + clippy 0 + a `perfs/data/.../metal-<n>-*.txt` recording its
  perf/mem/size delta (kept if it helps any axis; reverted+documented if it
  regresses all). On promotion, expand that checkpoint's L3b entry into a linear
  L3a hot plan.

Autorun: execute L3a; at each checkpoint completion check the L4 predicate, then
promote the next. No forks — a decision point means stop + return to L2.
