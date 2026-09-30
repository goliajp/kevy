# `kevy-alloc` — the server's allocator

Since 7.0 the kevy server runs on `kevy-alloc`, kevy's own pure-Rust span
allocator, instead of the system allocator (glibc malloc on Linux). It is
the `kevy-alloc` feature of the `kevy` crate, on by default, so every
server build — `cargo install kevy`, the release binaries, the container
image, `npm install -g @goliapkg/kevy-bin` — carries it.

Only the server binary installs it. A program that uses the `kevy` crate
as a library keeps whatever allocator it declares; linking the crate
changes nothing about how it allocates.

## What it does

- **One heap per shard, no locks on the fast path.** A shard allocates
  from its own thread-local heap. A block freed by another thread is sent
  back to the heap that owns it.
- **No per-block headers.** Rust passes the size on every free, so the
  allocator finds a block's span and size class from its address and
  stores nothing beside it.
- **Pages go back to the OS.** Occupancy is a bitmap outside the data
  pages, so any 4 KiB page that holds no live block can be returned
  (`madvise(MADV_DONTNEED)`) while its neighbours stay in use. Each shard
  tick returns the pages that have stayed free for a while. glibc's main
  arena can only shrink from the top: a free chunk below a live one stays
  resident.
- **Memory compaction.** A demoted or deleted value leaves a hole where it
  sat. While the free space inside a shard's spans is large next to what
  is live (above 1/64 of it, and above 4 MiB), the shard tick copies the
  values the allocator names into denser spans — 0.5 ms a tick, up to
  2 ms when the free space passes 1/16 — and the reclaim in the same tick
  returns the pages that emptied. The pass stops below 1/256. Values the
  store cannot move (an index leaf, a large collection) stay where they
  are.

## What it measured

**Throughput.** firefly (aarch64 Linux, 4 KiB pages), the same commit
built both ways, server pinned to 2 cores, `redis-benchmark -c 60`, five
rounds with the two builds rotated. Change against glibc:

| Command | Throughput |
|---|---:|
| `LPUSH` | +11.3 % |
| `ZADD` | +11.4 % |
| `HSET` | +3.1 % |
| `SADD` | +1.4 % |
| `GET`, `SET` | level |
| `INCR` | −2.2 % |

The `INCR` figure is noise: an exact instruction count (callgrind, lx64,
x86-64 Linux) puts it at 2015 instructions per command on kevy-alloc and
2016 on glibc. On the same count, every write command costs as many
instructions on kevy-alloc as on glibc, or fewer.

**Memory.** A tiered server holds its resident memory at the budget ×
1.05 (see [tiering.md](tiering.md)). On the D1 workload — ten million
hashes of about 1 KiB on a 3 GiB budget, two compiled indexes, lx64 —
glibc leaves about 3.5 % of the budget in holes it cannot return, which
the compaction pass packs and returns.

## What you see

`INFO modules` names the allocator the process runs on:

```
module:name=alloc,impl=kevy-alloc
```

`INFO allocator` splits every byte the allocator has mapped into named
terms, summed over the shards' heaps: `alloc_live` (in use),
`alloc_rounding` (size-class rounding), `alloc_cache`, `alloc_span_free`
(free slots inside spans in use — what compaction packs),
`alloc_returned` (given back to the OS), `alloc_virgin` (mapped, never
touched), `alloc_hysteresis` (emptied and held for reuse) and
`alloc_segment_overhead`. `alloc_accounted` is their sum and equals
`alloc_mapped`.

The tiering memory guard reads these figures instead of walking the
system heap, and has no heap to trim: `heap_trims_total` in `INFO
tiering` stays 0.

## Building without it

```
cargo build --release -p kevy --bin kevy --no-default-features
cargo install kevy --no-default-features
```

`kevy-alloc` is the crate's only default feature, so this build differs in
the allocator alone. It reports `module:name=alloc,impl=system`, has no
`# Allocator` section, and the tiering guard reads glibc's `mallinfo2`
(macOS: the malloc zone statistics) and trims the heap with `malloc_trim`
when freed memory piles up.

Reasons to build it:

- **Tools that hook malloc.** `LD_PRELOAD` allocators (jemalloc,
  tcmalloc), glibc's `MALLOC_*` tunables, heaptrack and valgrind's
  memcheck see malloc calls. The server's allocations do not go through
  malloc on the default build, so these tools see almost nothing.
- **Pages larger than 4 KiB.** The allocator returns pages at 4 KiB and
  refuses to return any where the system page is another size — Apple
  Silicon macOS (16 KiB), and arm64 Linux kernels built with 16 or 64 KiB
  pages. There it still reuses freed memory, but it gives none back to
  the OS.
- **A comparison.** To measure a workload of your own against the system
  allocator, build both and run them on the same box.
