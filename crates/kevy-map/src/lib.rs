//! `kevy-map` — a purpose-built open-addressing hashtable for kevy's keyspace.
//!
//! Per-shard, single-threaded, single-trust-domain. Trades `std::HashMap`'s
//! generality for three kevy-specific wins:
//!
//! 1. **Bucket-address API** ([`KevyMap::prefetch_for_hash`]) — lets the
//!    command-batch driver prefetch the next command's metadata group
//!    while finishing the current one.
//! 2. **No DoS-hardening tax** — single trust domain ⇒ no random seed.
//!    Hasher is `kevy_hash::KevyHash` (one-call inlinable).
//! 3. **Cache-conscious layout** — Swiss-style metadata bytes scanned 16 at
//!    a time (SSE2 / NEON, scalar elsewhere); slots AoS so the post-match
//!    key+value read hits one cache line.
//!
//! ```
//! use kevy_map::{KevyMap, KevySet};
//!
//! let mut m: KevyMap<Vec<u8>, u64> = KevyMap::new();
//! m.insert(b"user:1".to_vec(), 7);
//! // borrowed lookup: a byte slice finds a Vec<u8> key without allocating
//! assert_eq!(m.get(b"user:1".as_slice()), Some(&7));
//!
//! let s: KevySet<u64> = [3, 1, 3].into_iter().collect();
//! assert_eq!(s.len(), 2);
//! ```
//!
//! See the crate README for the design rationale.
//!
//! Constraints: pure Rust, no `crates.io` deps; `unsafe` is allowed here (scoped
//! to this crate) so `kevy-store` keeps `forbid(unsafe_code)`.

#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

// Renamed: this crate has its own `alloc` module (the raw-table
// allocation plumbing), so the alloc *crate* gets an alias.
extern crate alloc as alloc_crate;

mod alloc;
mod clone;
mod group;
mod grow;
mod into_iter;
mod iter;
mod map;
mod map_keyed;
mod raw_entry;
mod scan;
mod set;
mod slot;

pub use alloc::malloc_footprint;
pub use into_iter::IntoIter;
pub use iter::{Iter, IterMut, Keys, Values};
pub use kevy_hash::KevyHash;
pub use map::KevyMap;
pub use raw_entry::{RawEntryMut, RawOccupiedEntryMut, RawVacantEntryMut};
pub use set::{KevySet, SetIntoIter, SetIter};

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<KevyMap<u64, u64>>();
    send_sync::<KevySet<u64>>();
    send_sync::<IntoIter<u64, u64>>();
    send_sync::<SetIntoIter<u64>>();
    send_sync::<Iter<'static, u64, u64>>();
    send_sync::<IterMut<'static, u64, u64>>();
    send_sync::<Keys<'static, u64, u64>>();
    send_sync::<Values<'static, u64, u64>>();
    send_sync::<SetIter<'static, u64>>();
    send_sync::<RawEntryMut<'static, u64, u64>>();
};

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
