# mmkvgate ledger — kevy scalar fast path vs MMKV

The bar for the mobile door is MMKV's synchronous mmap get/set. This
ledger records the head-to-head as measured, **losing axes named, not
hidden** — the north star is to beat MMKV on every axis, and the honest
starting point is that we do not yet.

## Apple — iPhone 17 simulator, iOS 26.5 (M-series host)

XCTest `measure` (`bench/mmkvgate/apple`), 2000 ops over 200 warm keys,
per-op = mean / 2000. kevy `appendfsync = EverySec` (default) vs MMKV
mmap — comparable durability (both flush to the OS asynchronously, no
per-write fsync), so this is apples-to-apples.

**Simulator numbers, not device** — an iPhone 17 sim on an M-series Mac.
Relative standing (kevy vs MMKV in the same environment) is meaningful;
absolute ns are not a phone. Device XCTest + androidx-benchmark are the
next rungs.

| Axis        | kevy    | MMKV    | Ratio        | Verdict          |
|-------------|--------:|--------:|-------------|------------------|
| GET 16 B    | ~278 ns | ~425 ns | 0.65× (kevy)| **kevy 1.5× faster** |
| GET 256 B   | ~320 ns | ~500 ns | 0.64×       | **kevy 1.6× faster** |
| GET 4 KB    | ~455 ns | ~1450 ns| 0.31×       | **kevy 3.2× faster** |
| SET 16 B    | ~715 ns | ~565 ns | 1.27×       | MMKV 1.3× faster |
| SET 256 B   | ~1400 ns| ~675 ns | 2.07×       | MMKV 2.1× faster |
| SET 4 KB    | ~16 µs  | ~3.5 µs | 4.6×        | MMKV 4.6× faster |

### Reading it

- **GET: kevy wins every axis, and the lead widens with value size**
  (3.2× at 4 KB). kevy reads from an in-memory hashmap; MMKV reads
  through mmap + a protobuf decode, which scales with size.
- **SET: kevy loses every axis, and the gap widens with value size**
  (4.6× at 4 KB). kevy's SET appends the value to the AOF (a file
  `write` per set, plus the store insert + `Arc<[u8]>` copy); MMKV
  appends into an mmap page (no syscall, no copy out of the page cache).

## Attack log (perf-vs-foss)

SET is the losing family; the 4 KB axis scales worst. GET needs no
attack — it already clears the bar.

### Decomposition (perf-record on lx64, x86 ext4 + tmpfs)

`crates/kevy-embedded/examples/set4k_bench.rs` loops `Store::set` of a
4 KB value; `perf record` on the release build put **52% of SET-4KB
self-time in the `write` syscall** (`ksys_write` → `vfs_write` →
`generic_perform_write`), consistent on tmpfs and ext4. Root cause: the
AOF's `BufWriter<File>` defaulted to 8 KiB, so a 4 KB value fills it
every two appends and flushes a `write` syscall. MMKV's mmap append
pays no syscall — it memcpys into a mapped page.

### Attack #1 — AOF write buffer 8 KiB → 256 KiB (landed)

Amortise the `write` across ~64 appends instead of 2. Durability is
unchanged: `EverySec` still flushes + fsyncs once a second, so the
crash window stays ≤ 1 s regardless of buffer size.

A/B on lx64 (median of 3, 3M × SET 4 KB, ext4, EverySec, RSD < 1%):

| buffer | wall (3M sets) | per-op | vs baseline |
|--------|---------------:|-------:|------------:|
| 8 KiB  | 8.80 s | 2.93 µs | — |
| 256 KiB| 7.01 s | 2.34 µs | **−20.3%** |

`write` self-time 52% → 44% — the syscall *count* dropped, but the
per-`write` page-cache copy (`copy_from_user` + folio) scales with data
volume and is untouched. That residue is the next axis, and it is the
mmap-vs-write architectural gap (attack #2, larger): the copy into a
mapped page skips the syscall boundary and per-write folio lookup that
`write` pays. Also open: the value is copied **twice** per SET
(`value.to_vec()` into the store + `write_all` into the BufWriter) vs
MMKV's single in-page copy.

Effect on the mmkvgate 4 KB axis (proportional estimate): kevy SET 4 KB
~16 µs → ~12.8 µs, narrowing MMKV's lead from 4.6× to ~3.6×. Still
behind. This is a stone-layer win: every AOF write path (server SET
throughput, HSET/RPUSH/ZADD, bulk import) benefits, not just mobile.

### Decomposition #2 — where the remaining SET cost lives (lx64)

`set4k_bench` gained an AOF on/off toggle to isolate the store path
from the AOF append. 3M × SET 4 KB, median of 3, ext4:

| config           | wall | per-op | share |
|------------------|-----:|-------:|------:|
| AOF **off** (store only) | 0.70 s | 233 ns | 10% |
| AOF **on** (256 KiB buf) | 6.91 s | 2.30 µs | 100% |

**AOF is 90% of SET; the store (value.to_vec + hashmap insert) is a
flat 233 ns.** So the value copy into the store is a non-issue — the
whole gap is the AOF append. Of that append, perf splits ~43% `write`
syscall (ext4 page-cache: `copy_from_user` + folio) and ~47% user-space
(header formatting + the BufWriter memcpy of the value).

### Attack #2a — itoa header vs `write!` (REVERTED, no needle)

Replaced `write!(w, "*{}\r\n", n)` with a stack-buffer itoa header to
kill the `format_args!`/`write_fmt` dispatch. A/B: 6.91 s → 6.89 s —
within noise (~0.3%). Reverted. The header fmt is not the bottleneck;
the value memcpy (into the BufWriter) and the page-cache copy are, and
both scale with data volume, not with formatter overhead.

### Attack #2b — mmap append (the parity path, RFC-scoped, not yet done)

What is left is the architectural gap: kevy appends through
`write`→ext4 page-cache; MMKV memcpys into an mmap'd page. An
mmap-backed AOF (append = memcpy into a mapped region, grow by
ftruncate+remap, durability by msync on the EverySec tick) removes the
`write` syscall AND the per-write folio lookup, and lets the value be
copied once (into the mapped page) instead of into a BufWriter first.
Estimated to bring SET 4 KB from ~2.3 µs toward the ~0.3 µs store floor
— i.e. into MMKV's range. But it is a rewrite of the AOF backend
(mmap file lifecycle, growth, msync durability, replay + rewrite
compatibility, per-platform mmap), a persistence-core change that
belongs in its own RFC + isolated worktree + full durability/replay/
rewrite test pass — not a tail-end change. The decomposition above is
its ground truth; the checklist below is its starting point.

#### mmap-AOF implementation checklist (RFC scope)

1. **kevy-sys mmap binding** (none exists today). Hand-written
   `unsafe extern "C"` for `mmap` / `munmap` / `msync` / `ftruncate` —
   the same OS-boundary discipline as the socket/poller bindings, no
   `libc`/`memmap2` crate (the 0-dep charter). Unit-test the round trip.
2. **Aof mmap backend.** Replace `BufWriter<File>` with a mapped region
   + append offset: ensure capacity (grow via `ftruncate` + remap as
   the tail nears the end), memcpy the frame at `offset`, advance. One
   value copy (into the page) instead of two.
3. **Preserve every existing Aof semantic — the hard part, not the
   mmap.** `EverySec`/`Always`/`No` (msync replaces flush+fdatasync),
   the group-commit window, the `rewrite_tee` diff buffer,
   `rewrite_from` / `begin_concurrent_rewrite` (the mapped file is
   swapped), and `size_bytes` for the auto-rewrite trigger.
4. **Cross-platform.** mmap on Linux + macOS; wasm has no mmap — keep
   the `BufWriter<File>` backend as the target-gated fallback.
5. **Test pass.** kevy-persist + kevy-embedded replay + rewrite suites
   green; a crash-injection replay (kill mid-append, reopen, clean
   frame boundary); lx64 A/B (SET 4 KB toward the 233 ns store floor).

Until that lands, attack #1's 256 KiB buffer is the shipped SET win
(−20% on lx64, MMKV lead 4.6× → 3.6×).

**RFC written, awaiting 拍板:**
`.claude/plans/2026-07-15-v4-mmap-aof-rfc.md` turns this checklist into a
full design (kevy-sys mmap binding, backend trait with BufWriter
fallback, msync durability mapping every Fsync policy, growth strategy,
crash-consistency, and the merge-gate test pass). Two decisions block
implementation: go/no-go on the rewrite now, and the crash-EOF strategy
(committed-length marker vs zero-run-as-EOF).

### Re-measure on the simulator with attack #1 (KevyKit xcframework rebuilt)

Rebuilt the xcframework so KevyKit carries the 256 KiB buffer, re-ran
the iOS-sim matrix, and added a bulk axis (distinct keys, no reuse):

| Axis        | kevy (attack #1) | MMKV    | Ratio | vs pre-#1 |
|-------------|-----------------:|--------:|------:|----------:|
| SET 4 KB    | ~14 µs | ~3.5 µs | 4.0× | was 4.6×  |
| SET 256 B   | ~1.0 µs| ~0.5 µs | 2.0× | ~same     |
| BULK 4 KB   | ~15 µs | ~3.5 µs | 4.3× | (new)     |

Two honest reads:

- **attack #1 helps less on the simulator (~−12%) than on lx64 (−20%)**
  — the sim writes to the host filesystem, a different write path than
  lx64's ext4, and SET-4KB RSD is ~22% here, so the real-disk lx64
  number is the more trustworthy one. Still, the direction holds and
  the 4 KB lead closed from 4.6× to 4.0×.
- **The bulk hypothesis is refuted.** Distinct-key bulk (15 µs) is no
  better than warm-key SET (14 µs) — the buffer amortises the same way
  in both, so there is no extra bulk-only win. The remaining gap is the
  per-append page-cache/syscall cost, exactly what attack #2b (mmap)
  targets, in bulk and warm alike.
