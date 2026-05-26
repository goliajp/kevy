//! `kevy-map` — a purpose-built open-addressing hashtable for kevy's keyspace.
//!
//! Per-shard, single-threaded, single-trust-domain. Trades `std::HashMap`'s
//! generality for three kevy-specific wins:
//!
//! 1. **Bucket-address API** — `prefetch_for_hash` exposes the table's bucket
//!    metadata pointer so the command-batch driver can `prefetcht0` the next
//!    command's group while finishing the current one. This is the lever for
//!    `v0.metal-5` against the bucket-probe DRAM miss.
//! 2. **No DoS-hardening tax** — single trust domain ⇒ no random seed, no
//!    rehash-on-collision-storm. The hasher is `kevy_hash::KevyHash`
//!    (one-call inlinable, not `std::hash::Hasher`'s state machine).
//! 3. **Cache-conscious layout** — Swiss-style metadata bytes scanned 16 at a
//!    time (SSE2 on x86_64, SWAR u64 fallback elsewhere), slots stored AoS so
//!    the post-match key+value read hits one cache line.
//!
//! Design RFC: `rfcs/2026-05-26-kevy-map-design.md`.
//!
//! Charter: pure Rust, no `crates.io` deps; `unsafe` is allowed here (scoped
//! to this crate) so `kevy-store` keeps `forbid(unsafe_code)`.
//!
//! ## Status
//!
//! Scaffolding only. The core insert/get/remove/iter loop, the scalar group
//! scan, SSE2 fast path, and prefetch hook are scheduled in `perfs/METAL-
//! PLAN.md`'s L3a step list (`v0.metal-4+5`).

// Allow the unsafe that the table itself needs (raw slot init/drop, SSE2
// intrinsics). Other crates that depend on us stay safe.
#![deny(unsafe_op_in_unsafe_fn)]

pub use kevy_hash::KevyHash;

/// Placeholder so the crate compiles + tests run while the real impl is built
/// step-by-step on this feature branch. Will be replaced with the open-
/// addressing table proper in the next commit on this branch.
#[doc(hidden)]
pub struct _Stub;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kevy_hash_reexport_works() {
        // Smoke test that the trait is re-exported and the impls live in
        // kevy-hash. Real KevyMap tests follow once the table is implemented.
        let h = b"hello".kevy_hash();
        let h2 = b"hello".kevy_hash();
        assert_eq!(h, h2);
        assert_ne!(h, b"world".kevy_hash());
    }

    #[test]
    fn _stub_exists() {
        let _ = _Stub;
    }
}
