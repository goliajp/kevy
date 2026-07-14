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
behind — attack #2 (mmap append / kill the double copy) is the path to
parity. This is a stone-layer win: every AOF write path (server SET
throughput, HSET/RPUSH/ZADD, bulk import) benefits, not just mobile.
