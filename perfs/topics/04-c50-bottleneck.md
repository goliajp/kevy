# Topic 04: -c50 system-level bottleneck — client-bound, not a kevy ceiling

**Status:** fixed (single-key fast path) + ongoing (measurement is client-bound)
**Severity:** high (drove a months-long wrong conclusion)
**First observed:** 2026-05-26

## Symptom

The whole session believed kevy "lagged" valkey at -c50 (~0.9× / 16-shard 1.26M).
Clean isolated measurement (see `bench/REPORT.md`) already showed kevy *leads*
(io_uring 4.4M vs valkey 2.5M). This topic asks the next question: is 4.4M kevy's
ceiling or the load generator's?

## Reproduction

```
bash bench/perf_diag.sh   # Part A client-core scan, Part B shard scan (lx64)
bash bench/perf_sat.sh <binary> <label>   # server-bound A/B (few shards, strong client)
```

## Investigation log

- 2026-05-26 — **Part A (client-core scan, server = io_uring 10sh fixed on cores
  0-9), GET -c50 -P16:** client 1c→1.73M, 2c→3.11M, 4c→4.14M, 6c→4.68M.
  **Monotonic, not saturated at 6 cores** → the -c50 number is **CLIENT-BOUND**.
  kevy's server ceiling is **> 4.68M**; the 6-core redis-benchmark is the limit,
  not kevy. data: `data/2026-05-26/c50-diag.txt`.
- The earlier `--threads` shard-scan (1-shard 2.1M … 16-shard 1.26M) that looked
  like a "cross-core tax growing with core count" was the same artifact in
  reverse: more shards busy-poll more cores, starving a co-located client harder.
  See [[feedback-kevy-bench-isolation]].
- **Part B / perf_sat (server-bound: 4sh on 0-3, client 12c -P64):** kevy reaches
  **5.6M** here — *above* the 10sh/6c client-bound 4.68M, confirming the server
  has headroom the single-box client can't drive.

## Decision

1. **Single-key fast path (fixed):** `start_command` no longer allocates a
   `Vec<(shard, Op)>` for `Route::Local`/`Route::Single` (95%+ of commands) — it
   dispatches the single target inline (one `PendingSlot`, `args` moved straight
   into `Op::Dispatch` / `request_batch`). A/B (server-bound 4sh, GET/SET -c50
   -P64): **+1.6% GET / +3% SET**, no regression, and removes one heap alloc per
   command (matches the ~3.5%/shard the alloc cost predicts).
2. **Measurement (ongoing):** a single 16-core box cannot drive kevy's true -c50
   ceiling (client-bound). For client-independent server-CPU optimization, use a
   **component in-process micro-bench** (mailrs method, host-robust) and/or a
   second physical load-gen box. perf_sat (server-bound) is the next-best
   end-to-end proxy.

## Verification

io_uring sharded suite 11/11 (epoll + io_uring) with the fast path; clippy 0.
perf_sat A/B numbers above. data: `data/2026-05-26/c50-diag.txt`,
`data/2026-05-26/fastpath-sat-ab.txt`.

## Follow-up: pipeline scan classifies the bottleneck (2026-05-26)

`bench/perf_pipe.sh` (4sh io_uring, 12-core client, GET -c50, vary -P):

| -P | rps |
|----|----:|
| 1 | 377k |
| 4 | 1.38M |
| 16 | 3.33M |
| 64 | 5.85M |
| 256 | 7.21M |

**19× from -P1→-P256 ⇒ per-command syscall/reactor overhead dominates, not
command CPU** (~120ns CPU vs 555ns/cmd at -P256 single-shard). And -P256 reaches
7.2M on the *same* 4-shard server that caps at 3.33M for -P16 ⇒ **-P16 4sh is
server-bound** (clean A/B config). data: `data/2026-05-26/pipeline-scan.txt`.

→ **Next lever (topic 05): io_uring multishot recv + provided-buffer ring.** At
-P16 there are ~16× more read SQEs than at -P256; multishot re-fires one recv per
connection and lets the kernel pick a buffer, cutting submit/re-arm overhead.
~2× headroom at the typical -P16.

## Definitive grounding: single-shard ceiling (2026-05-26)

1 shard on core 0 + a 15-core client (load generator definitively not the limit),
GET -c50 -P256: **epoll 3.77M / io_uring 3.76M rps** (`data/.../single-shard-ceiling.txt`).

- **Per-core GET ceiling ≈ 3.77M (~265 ns/cmd)** — already ~1.5× valkey 9.1's
  *total* throughput (single-threaded exec).
- The 4-shard 7.2M (1.8M/shard) was **client-bound**, not a server limit: one shard
  alone does 3.77M. **The dominant untapped headroom is multi-core scaling**
  (~10-13 shards × 3.77M = 38-49M GET/s on this box) — unreachable by any
  single-box co-located client. **A dedicated load-gen machine is the gating need.**
- Per-core ~265 ns/cmd ≈ 110 ns command CPU + ~150 ns reactor/io_uring; the latter
  is the only single-box-measurable lever left, with modest/complex remaining knobs.
