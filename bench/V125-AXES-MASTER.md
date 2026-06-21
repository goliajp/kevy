# v1.25 perf axes — deep-dive master plan

After the v1.24 L1 + L2 + threads-tune sprint hit the "default
redis-benchmark workload" userspace ceiling (c1-P1 ≥120 % vs valkey,
concurrent scenarios 99-119 %), this document tracks a systematic
science pass over **six workload axes** chosen to expose kevy's
structural strengths.

Hypothesis-driven, not opportunistic. Each axis has:
- a clear **why** (which kevy invariant should dominate)
- a **methodology** (cmdline, pinning, run count, sample policy)
- a **raw + pivoted dataset**
- an **interpretation** (where the win/loss lives in code)
- a **threshold scan** (at what input parameter does kevy cross
  120 %?)

## Axis catalogue

| ID | Axis | kevy structural lever | Methodology | Status |
|----|------|----------------------|-------------|--------|
| A | **Deep pipelining** (`-P 1/4/16/64/256`) | io_uring multishot recv + E14 enter-skip + L1 writev = true async-batch; epoll is sync-polled per-iter | `redis-benchmark -c 50 -P {1,4,16,64,256} -t set,get` | pending |
| B | **Big value sweep** (`-d 64 / 256 / 1K / 4K / 16K / 64K`) | L1 `Value::ArcBulk` + writev iovec → zero memcpy; valkey's `tryAvoidBulkStrCopyToReply` also zero-copy but jemalloc-fragmented robj chain | `redis-benchmark -c 50 -P 1 -t set,get -d N` | pending |
| C | **High key churn** (`-r 100k / 1M / 10M random keys`) | SmallBytes inline (≤22 B) = 0 malloc per SET; valkey allocates a `robj` per SET regardless | `redis-benchmark -c 50 -P 1 -t set -r N` | pending |
| D | **Large keyspace ops** (`SET 10M then GET random`) | E13 2 MiB-aligned mmap THP keyspace (`AnonHugePages: 40 960 kB`) vs valkey's default-page dict | warm 10 M SET → GET-only with -r 10M | pending |
| E | **Deep concurrency** (`-c 50 / 200 / 500 / 1000 / 5000`) | shared-nothing thread-per-core + kevy-ring SPSC scales linearly; valkey's single-dispatcher saturates beyond ~1 k conns | sweep -c with --threads matched per workload | pending |
| F | **Embedded** (`kevy_embedded::Store` direct calls) | kevy unique — no network, no protocol, no kernel — pure in-process keyspace at 9 M GET/s. valkey has NO embedded mode (category-uncomparable but worth showing). | `cargo run -p kevy-embedded --example bench_matrix --release` (new) | pending |

## Common methodology

- **Host**: lx64 (Intel i7-10700K Comet Lake, 16 cores, kernel 6.12,
  mitigations=off — see `cat /proc/cmdline`).
- **Built from source**:
  - kevy v1.25-pending @ `/root/kevy/target/release/kevy`
  - valkey 9.1.0 @ `/root/srcbench/valkey/src/valkey-server`
  - redis 8.8.0 @ `/root/srcbench/redis/src/redis-server`
  - client: `redis-benchmark` from redis 8.8.0 (the canonical Redis
    benchmark client — same client hits all three servers)
- **Pinning**: server on cores 0..N-1 (with N tuned per axis — kevy
  `--threads 2` for low-conn axes, `--threads 8-16` for high-conn);
  client on cores 10..13.
- **Persistence**: all servers `--save '' --appendonly no` (in-memory).
- **Warm-up**: 50 000 SETs before every measurement run.
- **Sample policy**: 3 runs per scenario, **median** reported; min and
  max also kept in the raw TSV so noise floor is auditable.
- **Box state**: before every axis run, `pgrep -af kevy|redis-server|
  valkey-server` confirmed clean.
- **Honest reporting**: scenarios where kevy LOSES are kept in the
  table with the loss reason called out. No cherry-picking; the table
  shows every cell.

## "Win" definition

`kevy_rps / max(valkey_rps, redis_rps) >= 1.20` per scenario.

- **≥120 %** = win (the user's bar)
- **100-119 %** = lead but below bar
- **<100 %** = loss

## Status board — all seven axes complete (2026-06-21)

| ID | hypothesis | result | crossing | peak headline |
|----|-----------|--------|----------|----------------|
| **A** | io_uring batched-async wins as -P grows | ✅ **CONFIRMED** | -P 64 | **kevy 411 % SET / 366 % GET at -P 256** (11.77 M GET/s) |
| **B** | L1 ArcBulk + writev wins big-value GET | ❌ not confirmed | — | tied with valkey across 64 B → 64 KB (valkey already zero-copies); kevy LOSES -3-5 % at 64 KB |
| **C** | SmallBytes inline wins SET churn | ❌ not confirmed | — | tied at 99-100 % across 100k / 1M / 10M keyspace (malloc savings sub-noise at c50-P1 RTT) |
| **D** | E13 THP keyspace wins TLB | ⚠ **mechanism verified, bench shape doesn't expose** | — | `AnonHugePages=588 MiB` confirmed at 10 M keys; bench TIED 99 % (RTT-bound hides TLB savings) |
| **E** | shared-nothing wins at high conn count | ❌ **BUSTED — opposite held** | — | **kevy LOSES from -c 500** (95 %), worsens to **-c 2000 → 80 % (-20 %)** after fast-skip + Vec-walk mitigations — `arm_conns` cliff is fundamentally kernel-scaling-bound at high conn |
| **F** | kevy-unique embedded mode | ✅ **CONFIRMED unique** | n/a | **9.15 M GET/s, 8 M INCR/s, 38 M GET-miss/s** in-process. valkey has no embedded mode at all. |
| **G** | KevyMap vs listpack/dict gives collection edge | ❌ not confirmed | — | tied across SADD/HSET/ZADD/LPUSH/RPUSH/LRANGE_{100,300,600} at 99-103 % vs valkey |

## Headline results vs the ≥120 % bar

Where kevy now hits ≥120 % vs the best competitor:
- **c1-P1 SET/GET** (v1.24 matrix): 122 % / 120 %
- **Axis A -P 64+**: 308 % SET / 223 % GET (Axis A)
- **Axis A -P 256**: 411 % SET / 366 % GET
- **Axis F** in-process: ∞ (category-unique; valkey has no embedded)

Where kevy is at parity (99-119 %) vs valkey:
- c50/c100 small-value (matrix), Axis B (big value), Axis C (churn), Axis D (10M keyspace)

Where kevy LOSES:
- **Axis E c ≥ 500**: -5 % to -27 %. Root cause known (iterate-all
  `arm_conns`); fix is a ready-set bitmap, deferred to follow-up
  sprint.
- Some matrix concurrent scenarios -1 to -5 % (within bench noise)

## What we learned about kevy's structural positioning

**kevy wins decisively on the workloads where its architecture choices
matter**:
- io_uring batched async (Axis A pipelining)
- in-process / no-network (Axis F)
- single-conn round-trip latency (c1-P1 in matrix)

**kevy ties valkey on workloads where valkey's mature epoll +
zero-copy + tcache design has already absorbed the relevant
optimisations** (Axes B, C, D).

**kevy LOSES at high conn count** (Axis E) due to iterate-all
busy-poll — a known structural choice that traded high-conn
scalability for low-conn latency. The fix is the ready-set bitmap
refactor (a clear future-sprint item).

## Deep-dive docs

- `V125-AXIS-A-PIPELINE.md` — pipelining sweep (kevy wins big)
- `V125-AXIS-B-BIGVAL.md` — big value sweep (tied)
- `V125-AXIS-C-CHURN.md` — high key churn (tied)
- `V125-AXIS-D-KEYSPACE.md` — large keyspace TLB (verified-working, bench-invisible)
- `V125-AXIS-E-CONCURRENCY.md` — deep concurrency (kevy loses — known cause, fast-skip + Vec-walk mitigations applied)
- `V125-AXIS-F-EMBEDDED.md` — embedded mode (kevy-unique capability)
- `V125-AXIS-G-COLLECTIONS.md` — collection ops (tied across 8 ops)

Each doc includes raw data, methodology, interpretation, and
reproducibility instructions.

## Final v1.25 sprint summary

**Where kevy hits ≥120 % vs the best competitor:**
1. **Axis A `-P 64+`** — 308 % SET / 223 % GET at -P 64, peaking
   **411 % / 366 %** at -P 256. io_uring batched async is kevy's
   killer architecture lever.
2. **c1-P1** (v1.24 matrix) — 122-126 % SET/GET. Busy-poll
   single-conn latency.
3. **Axis F embedded** — 9 M+ ops/s in-process, kevy-unique
   (valkey has no comparable mode).

**Where kevy ties valkey (99-119 %)**: Axes B/C/D/G + most matrix
concurrent scenarios. Valkey's mature epoll + tcache + listpack
have absorbed the same optimisations kevy brings.

**Where kevy loses**: Axis E at c ≥ 500 (95 → 80 % vs valkey at
c=2000). Mitigations (fast-skip, Vec-walk) closed marginal cost
but the root issue is kernel per-flow scaling. The arm_conns
ready-set queue is a known fix path for a follow-up sprint;
beyond that, only valkey-style architecture pivot would close it.

**Net positioning for v1.25**:

kevy wins **decisively** on:
- Pipelined throughput (Axis A — 4-5× valkey at -P 256)
- Single-conn latency (matrix c1-P1)
- In-process embedded (Axis F — kevy-unique)

kevy is **at parity** on:
- Small-value c=50-200 concurrent (matrix + Axes B/C/G)
- Big-value GET (Axis B, after L1 zero-copy)
- Large keyspace lookup (Axis D, with verified THP)

kevy **loses** on:
- High-conn-count (c ≥ 500) — known structural limitation

This is **the architecturally honest positioning** of kevy v1.25
against the current state-of-the-art (valkey 9.1, redis 8.8) on
loopback with all servers source-built and pinned identically.
