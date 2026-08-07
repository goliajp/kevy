# kevy-alloc

A per-shard, mmap-backed, **header-free** allocator, written for kevy's
share-nothing engine. Pure Rust, zero dependencies, `no_std`-friendly.

> **Status: experimental.** This is part of an ongoing v5 experiment, not
> a settled design. Its premises are under test and may change. It is not
> wired into kevy yet.

## Why

Tiering holds kevy's *logical* memory at its budget, but resident memory
ran **2.24×** that bound on ~400 B values. The cause is not tuning:
glibc's `brk` arena only shrinks from the top, so a freed chunk beneath a
live one is a page the OS never gets back. `malloc_trim(0)` and
`MALLOC_ARENA_MAX=2` were both measured and neither moved it at all.

For the small companies kevy is aimed at, RAM is the budget line, so that
ratio decides how much business fits on the box they already have.

## The idea

A general-purpose allocator serves C's `free(ptr)`, which carries no
size, so it must store one beside every chunk — and those interleaved
headers are part of why the heap cannot shrink. Rust hands us the
`Layout` on deallocation.

**This allocator serves sized deallocation only, so it stores no headers
at all.** A pointer's segment, span and size class are recovered by
masking the address:

```text
segment = ptr & !(4 MiB - 1)      // 4 MiB, mapped 4 MiB-aligned
span    = (ptr & (4 MiB - 1)) / 64 KiB
class    = segment.spans[span].class
```

Memory comes from `mmap`. Occupancy is a **bitmap in the segment
header** — data pages hold zero metadata, so reclaim works at **page
granularity**: any 4 KiB page no live slot overlaps goes back with
`madvise(MADV_DONTNEED)` while its neighbours stay live. Allocation is
lowest-first, which densifies — live slots pack low, churn migrates free
space upward into whole returnable pages.

## Accounting

Every mapped byte is in exactly one of seven states, and the identity is
exact rather than approximate:

```text
mapped == live + rounding + cache + span_free + virgin
        + hysteresis + segment_overhead
```

`Stats::balanced()` asserts it. Only `rounding` scales with the data;
`virgin` is mapped-but-never-touched, so it is address space rather than
memory. The full contract is `bench/V5-ACCOUNTING-CONTRACT.md`.

## Measured

Apple M4 Max, `--release`, medians over N samples
(`cargo run -p kevy-bench --release --example stones -- alloc`):

| shape | kevy-alloc | system |
|---|---:|---:|
| alloc+free 64 B | 5 ns | 10 ns |
| alloc+free 400 B | 5 ns | 18 ns |
| alloc+free 4096 B | 5 ns | 16 ns |
| churn 4096 × 400 B, interleaved free | 3.8 ns/op | 19.5 ns/op |
| the same, **plus returning the pages** | 29.3 ns/op | — |

The last row has no system column because there is nothing to compare it
to: that is the operation glibc cannot perform at any price.

## Usage

```rust
use kevy_alloc::Heap;

let mut heap = Heap::new(0); // one heap per shard
if let Some(p) = heap.alloc(400, 8) {
    // SAFETY: from this heap, with this size and alignment.
    unsafe { heap.dealloc(p, 400, 8) };
}
heap.reclaim();               // return empty spans to the OS
assert!(heap.snapshot().balanced());
```

`dealloc` must be given the same size and alignment as `alloc` — that is
Rust's `Layout` contract, and it is what buys the missing headers.

## Standing on shoulders

mimalloc (segment/page geometry, the push-only thread-free list),
tcmalloc (graded size classes), the Go runtime (span ownership, heap
accounting as a first-class export), jemalloc (decay before returning
pages), and torajs-mmalloc — including two lessons it paid for: an
uncapped span pool is a SIGSEGV rather than a leak, and a cutover without
a fast path costs 10–30 ns per allocation.

Where we step off: those allocators put a thread cache in front of a
shared heap because they cannot know how threads relate to memory. kevy
pins a shard per core, so the heap *is* thread-local and the fast path is
already free of atomics. There is no cache in front of this.

## License

MIT OR Apache-2.0
