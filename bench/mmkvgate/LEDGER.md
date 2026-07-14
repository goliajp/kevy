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

## Attack surface (perf-vs-foss)

SET is the losing family. The 4 KB axis (4.6×) is where the
size-scaling cost lives, so that is the decomposition target: the
per-set AOF `write` and the value copy on the SET path vs MMKV's
in-page append. Phase A (read both paths side by side, ±20% budget)
before any change. Tracked as the next mmkvgate step.

GET needs no attack — it already clears the bar.
