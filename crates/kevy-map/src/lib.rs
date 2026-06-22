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

mod alloc;
mod clone;
mod group;
mod iter;
mod map;
mod map_keyed;
mod raw_entry;
mod set;

pub use kevy_hash::KevyHash;
pub use iter::{Iter, IterMut, Keys, Values};
pub use map::KevyMap;
pub use raw_entry::{RawEntryMut, RawOccupiedEntryMut, RawVacantEntryMut};
pub use set::{KevySet, SetIter};
