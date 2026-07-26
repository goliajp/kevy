//! `kevy-alloc` — a per-shard, mmap-backed, header-free allocator.
//!
//! # Why this exists
//!
//! Tiering holds kevy's *logical* memory at its budget, but resident
//! memory ran 2.24× that bound on ~400 B values and 1.65× on 4 KiB ones.
//! The cause is not tuning: glibc's `brk` arena only shrinks from the
//! top, so a freed chunk under a live one is a page the OS never gets
//! back. `malloc_trim(0)` and `MALLOC_ARENA_MAX=2` were both measured
//! and both moved it by nothing
//! (`bench/PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md`).
//!
//! For the small companies kevy is aimed at, RAM is the budget line, so
//! that ratio decides how much business fits on the box they already
//! have.
//!
//! # What we get from a narrower contract
//!
//! A general-purpose allocator serves C's `free(ptr)`, which carries no
//! size, so it must store one beside every chunk — and those interleaved
//! headers are part of why the heap cannot shrink. Rust hands us the
//! `Layout` on deallocation. **This allocator serves sized deallocation
//! only, so it stores no headers at all**: a pointer's segment, span and
//! class are recovered by masking the address (see [`segment`]).
//!
//! # Status
//!
//! Part of an experiment, not a settled design. Every claim here is
//! under test, and a premise that measurement kills gets changed rather
//! than worked around — see `.claude/rfcs/2026-07-26-v5-kevy-alloc.md`
//! and ROADMAP rule ⑤. The gate is `bench/allocgate.sh`; the accounting
//! it checks is fixed by `bench/V5-ACCOUNTING-CONTRACT.md`.
//!
//! # Standing on shoulders, and where we step off
//!
//! - **mimalloc** — segment/page geometry, and the push-only thread-free
//!   list that makes cross-thread frees ABA-free by construction.
//! - **tcmalloc** — graded size classes; see [`class`] for why eight
//!   subdivisions per octave rather than four.
//! - **Go runtime** — span ownership, and heap accounting as a
//!   first-class exported thing rather than a debug aid.
//! - **jemalloc** — decay-style hysteresis before returning pages.
//! - **torajs-mmalloc** — a working mmap-backed realisation of all of
//!   the above, plus two lessons it paid for: a missing per-class cap is
//!   a SIGSEGV rather than a leak, and a cutover without a fast path
//!   costs 10–30 ns per allocation.
//!
//! The step off: those allocators put a thread cache in front of a
//! shared heap because they cannot know how threads relate to memory.
//! kevy pins a shard per core, so the heap *is* thread-local and the
//! fast path is already atomic-free. See [`heap`].
//!
//! # Example
//!
//! ```
//! # use kevy_alloc::Heap;
//! let mut heap = Heap::new(0);
//! if let Some(p) = heap.alloc(400, 8) {
//!     // SAFETY: `p` came from this heap with this size and alignment.
//!     unsafe { heap.dealloc(p, 400, 8) };
//! }
//! let stats = heap.snapshot();
//! assert!(stats.balanced(), "every mapped byte must be accounted for");
//! ```

// The `global` feature needs thread-local storage, which is a `std`
// facility; the core allocator itself is `core`-only.
#![cfg_attr(all(not(test), not(feature = "global")), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod class;
#[cfg(feature = "global")]
pub mod global;
pub mod heap;
pub mod large;
pub mod os;
pub mod pagemap;
mod reclaim;
pub mod segment;
mod snapshot;
pub mod stats;

#[cfg(feature = "global")]
pub use global::{KevyAlloc, thread_reclaim, thread_stats};
pub use heap::{EMPTY_SPAN_HYSTERESIS, Heap, PER_CLASS_CAP};
pub use large::large_stats;
pub use stats::Stats;

#[cfg(test)]
mod tests;
