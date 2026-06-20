//! Cache-line-padded wrapper for cross-shard atomics. Introduced by the
//! A2 attack (2026-06-20) after the H1 `perf c2c` diagnostic showed
//! `Arc<AtomicU64>` allocations in `inbound_dirty` landing on adjacent
//! cache lines — cross-shard `fetch_or` from the sender and `swap` from
//! the owner bounced the line between cores. Padding each atomic to a
//! full 64-byte line eliminates the bounce.

/// 64-byte-aligned wrapper around an atomic to prevent false sharing.
///
/// 64 bytes matches the x86_64 L1d cache-line size; aarch64 N1 / N2 / V1
/// are also 64. (Comet Lake reference: H1 validated L1d-miss = 24.6%
/// of backend stalls.)
///
/// Use `Arc<CachePadded<T>>` for atomics that are touched cross-shard
/// (currently `inbound_dirty` and `parked`). The inner `T` is reachable
/// via `Deref`, so call sites like `arc.load(...)` / `arc.swap(...)`
/// auto-deref through the wrapper unchanged.
#[repr(align(64))]
pub(crate) struct CachePadded<T>(pub(crate) T);

impl<T> CachePadded<T> {
    pub(crate) fn new(inner: T) -> Self {
        Self(inner)
    }
}

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
