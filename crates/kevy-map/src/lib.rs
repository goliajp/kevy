//! `kevy-map` — a purpose-built open-addressing hashtable for kevy's keyspace.
//!
//! Per-shard, single-threaded, single-trust-domain. Trades `std::HashMap`'s
//! generality for three kevy-specific wins:
//!
//! 1. **Bucket-address API** (`prefetch_for_hash`, future) — exposes the
//!    table's bucket metadata pointer so the command-batch driver can
//!    `prefetcht0` the next command's group while finishing the current.
//! 2. **No DoS-hardening tax** — single trust domain ⇒ no random seed.
//!    Hasher is `kevy_hash::KevyHash` (one-call inlinable).
//! 3. **Cache-conscious layout** — Swiss-style metadata bytes scanned (scalar
//!    in this commit; SSE2 group scan lands in a later pass); slots
//!    AoS so the post-match key+value read hits one cache line.
//!
//! See the crate README for the design rationale.
//!
//! Constraints: pure Rust, no `crates.io` deps; `unsafe` is allowed here (scoped
//! to this crate) so `kevy-store` keeps `forbid(unsafe_code)`.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

// Renamed: this crate has its own `alloc` module (the raw-table
// allocation plumbing), so the alloc *crate* gets an alias.
extern crate alloc as alloc_crate;

mod alloc;
mod clone;
mod group;
mod iter;
mod map;
mod map_keyed;
mod raw_entry;
mod scan;
mod set;

pub use iter::{Iter, IterMut, Keys, Values};
pub use kevy_hash::KevyHash;
pub use map::KevyMap;
pub use raw_entry::{RawEntryMut, RawOccupiedEntryMut, RawVacantEntryMut};
pub use set::{KevySet, SetIter};

/// Loop counts for the heavy unit tests, scaled down under miri.
///
/// miri interprets every memory access. Five tests in this crate — the
/// scan sweeps and the two large probe-chain tests — were 339 of the miri
/// job's 389 seconds, and that job alone is the CI wall clock: 10.5 of
/// 10.5 minutes, with 57 other jobs finishing inside its shadow.
///
/// What miri checks here is undefined behaviour in the probe, grow,
/// tombstone and clone paths. Those are entered the same way at 1,000
/// keys as at 10,000; the count buys coverage of the *logic*, and the
/// native `cargo test` run still does that at full size. Each test keeps
/// its structural assertion — two capacity doublings, no unexpected
/// grow, every key visited exactly once — so a count too small to force
/// the path it needs FAILS the test rather than quietly passing on less.
#[cfg(test)]
pub(crate) const fn scaled(full: usize) -> usize {
    if cfg!(miri) {
        let n = full / 10;
        if n < 64 { 64 } else { n }
    } else {
        full
    }
}
