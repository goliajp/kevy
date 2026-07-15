# PERF FINDING — mmap-AOF is a 6× SET regression, not a speedup (REFUTED)

**Date:** 2026-07-15 · **Env:** lx64 (x86_64, 16-core, ext4, kernel 6.12)
**Method:** perf-vs-foss decomposition. Branch `feature/v4-mmap-aof` (correct,
all tests green) A/B'd against the shipped BufWriter backend.
**Verdict:** the mmap AOF backend is **5–6× SLOWER** than the buffered
backend at every meaningful scale. Do NOT merge. The MMKV SET advantage is
architectural (overwrite-in-place small store), not an mmap trick.

## The A/B (single binary, format-preserving backend select, median of 3)

`set4k_bench <dir> 4096 <N>` — fresh dir → KEVYAOF2/mmap; pre-seeded
`KEVYAOF1` → BufWriter. Magic verified post-run (backends really switched).

| N (≈AOF size) | mmap µs/op | buffered µs/op | mmap/buf |
|---------------|-----------:|---------------:|---------:|
| 5 000 (~20 MB)| 23.0       | 21.4           | 0.93× (startup-bound) |
| 50 000 (~200 MB)| 16.2     | 4.78           | 0.30× |
| 500 000 (~2 GB)| 18.6      | 3.00           | 0.16× |
| 3 000 000 (~12 GB)| 17.6   | 3.05           | 0.17× |

**Buffered amortises to ~3 µs/op steady state; mmap is a FLAT ~17 µs/op at
every size.** Flat-per-op means the cost is per-operation, not a large-file
writeback pathology.

## Mechanism — confirmed with `perf stat` (500k × 4 KB)

| | page faults | user | sys | wall |
|--|-----------:|-----:|----:|-----:|
| mmap     | **528,823** (≈1.06 / append) | 7.00 s | 2.44 s | 8.55 s |
| buffered | 4,816 (≈0.01 / append)       | 0.39 s | 0.97 s | 1.46 s |

**An append-only mmap touches a fresh page every ~4 KB append and takes a
minor page fault on each** — 528k faults for 500k appends. `write()`
populates the page cache in kernel-batched fashion with almost none (4.8k).
mmap also burns ~18× the user-CPU (the per-byte CRC32 for the committed_len
marker + per-fault handling).

## Why MMKV's mmap wins but kevy's can't

MMKV mmaps a **small, resident, overwrite-in-place** key-value blob: the
pages stay hot, the file doesn't grow, and a SET rewrites the same region —
zero faults after warmup. kevy's AOF is an **unbounded append-only replay
log**: it never revisits a page, so it faults once per append forever. The
two are different data structures; mmap is right for the former and wrong
for the latter. Their SET-speed difference is the **durability model**
(overwrite-in-place, no history vs append-log with full replay/CDC), not the
mapping mechanism. No amount of mmap-implementation polish changes the
1-fault-per-append floor of an append-only log.

## Recommendation

1. **Do not merge `feature/v4-mmap-aof`.** Keep it as a documented negative
   result. The shipped `BufWriter` backend (with the `31231d1d`
   torn-tail-truncate crash fix) is the correct tool for an append log.
2. The mmkvgate SET gap is **architectural**. To actually match MMKV's SET
   would require an **overwrite-in-place scalar fast-path** for hot scalar
   keys (MMKV's shape), which sacrifices the AOF's replay/CDC/history for
   those keys — a real product decision, not an implementation tweak.
3. The mmkvgate SET numbers were measured on an **iOS simulator** (whose
   host-FS write path the LEDGER already flags as untrustworthy). On real
   ext4 (lx64) the buffered SET 4 KB is ~3 µs/op — much closer to MMKV's
   ~3.5 µs than the sim's ~14 µs suggested. **A real-device mmkvgate is the
   honest next measurement** before committing to an overwrite-in-place
   rearchitecture.

## The phantom gap — kevy already BEATS MMKV on SET on real hardware

After refuting mmap, built MMKV's own POSIX/Core lib on lx64 and ran the
**same** SET workload (200 warm keys, 4 KB value, 500k sets) through both,
on the same ext4, external timing, median of 3:

| SET 4 KB (lx64 ext4) | µs/op | sets/s | on-disk |
|----------------------|------:|-------:|---------|
| **kevy buffered (EverySec)** | **2.98** | 335,345 | 5.4 MB (auto-rewrite compacts) |
| **MMKV (default)** | 11.84 | 84,445 | 2.1 MB (overwrite-in-place) |

**kevy is 3.97× FASTER than MMKV at SET on real hardware.** The iOS-sim
mmkvgate said the opposite (kevy ~14 µs, MMKV ~3.5 µs, MMKV 4× faster) —
that was a **simulator artifact**: the sim's host-FS write path both
inflated kevy's `write()` and made MMKV's mmap look cheap. On real ext4,
MMKV's mmap + periodic full-writeback loses to kevy's `write()` + BufWriter
+ auto-rewrite. Both compact (kevy via auto-BGREWRITEAOF keeping the AOF at
5.4 MB, MMKV via in-place overwrite at 2.1 MB); kevy's is 4× faster.

**The whole mmap-AOF detour was chasing a gap that does not exist on real
hardware.** On real hardware kevy beats MMKV on *both* GET and SET.
Durability note: kevy ran EverySec (fsync ≤1 s), MMKV its default (lazy
writeback + sync-on-close) — kevy is the *more* durable of the two and
still 4× faster.

Caveat (honesty): lx64 is x86_64 Linux ext4, not a phone (ARM + flash).
This refutes "MMKV is fundamentally faster at SET" (it isn't — that was
sim-specific), but the definitive **mobile** number still needs a
real-device mmkvgate. The sim is now the outlier of three measurements
(sim: MMKV wins; lx64: kevy wins 4×), so its numbers should not drive
decisions.

## What was NOT wasted

The mmap agent's work is a correct, tested implementation (kevy-sys mmap
binding, committed_len+crc crash-safety, all rewrite paths) — the negative
result is about the *architecture fit*, not the code. The kevy-sys mmap
binding is reusable if a future feature genuinely wants a resident mapped
region (a snapshot mmap-read path, say). The branch is preserved.
