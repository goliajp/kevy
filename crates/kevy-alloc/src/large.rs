//! The direct-mapping path.
//!
//! Requests past the largest size class get their own mapping and give
//! it straight back on release: no pooling, no span, and therefore no
//! slack — a direct mapping is exactly as big as it needs to be, page
//! rounding aside.
//!
//! # Why these counters are per process and not per heap
//!
//! A large block has no segment, so nothing records which heap it came
//! from, so a free arriving on another thread has nowhere to be routed.
//! Whichever thread frees it is the one that unmaps it. Per-heap
//! counters would therefore drift negative the first time a large
//! allocation crossed a thread — the same defect the small path settles
//! through the foreign list, but with nowhere to settle it. The process
//! figure is the one that is meaningful, so it is the one kept, and
//! [`large_stats`] stays out of `Heap::snapshot` so that summing shards
//! cannot count it once per shard.

use core::ptr::NonNull;

use crate::os;
use crate::stats::Stats;

/// The counters themselves. See the module docs for why they live here.
mod counters {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub(super) static MAPPED: AtomicU64 = AtomicU64::new(0);
    pub(super) static LIVE: AtomicU64 = AtomicU64::new(0);
    pub(super) static ROUNDING: AtomicU64 = AtomicU64::new(0);
    pub(super) static COUNT: AtomicU64 = AtomicU64::new(0);

    pub(super) fn add(mapped: u64, requested: u64) {
        MAPPED.fetch_add(mapped, Relaxed);
        LIVE.fetch_add(requested, Relaxed);
        ROUNDING.fetch_add(mapped - requested, Relaxed);
        COUNT.fetch_add(1, Relaxed);
    }

    pub(super) fn sub(mapped: u64, requested: u64) {
        MAPPED.fetch_sub(mapped, Relaxed);
        LIVE.fetch_sub(requested, Relaxed);
        ROUNDING.fetch_sub(mapped - requested, Relaxed);
        COUNT.fetch_sub(1, Relaxed);
    }
}

/// Direct-mapping figures for the whole process.
///
/// Kept apart from [`Heap::snapshot`] rather than folded in, because
/// summing per-shard snapshots would then count them once per shard.
/// Each balances on its own, and so does their sum.
#[must_use]
pub fn large_stats() -> Stats {
    use core::sync::atomic::Ordering::Relaxed;
    Stats {
        mapped: counters::MAPPED.load(Relaxed),
        live: counters::LIVE.load(Relaxed),
        rounding: counters::ROUNDING.load(Relaxed),
        large_count: counters::COUNT.load(Relaxed),
        ..Stats::default()
    }
}

/// Map `size` bytes directly. `None` when the OS refuses or the
/// alignment is stricter than a fresh mapping provides.
pub(crate) fn alloc(size: usize, align: usize) -> Option<NonNull<u8>> {
    if align > os::PAGE {
        return None;
    }
    let mapped = os::round_up(size, os::PAGE);
    let p = os::map_aligned(mapped, os::PAGE)?;
    counters::add(mapped as u64, size as u64);
    Some(p)
}

/// # Safety
/// `ptr`/`size` must come from [`alloc`] and not be used afterwards.
pub(crate) unsafe fn dealloc(ptr: NonNull<u8>, size: usize) {
    let mapped = os::round_up(size, os::PAGE);
    counters::sub(mapped as u64, size as u64);
    // SAFETY: delegated to the caller's contract.
    unsafe { os::unmap(ptr, mapped) };
}
