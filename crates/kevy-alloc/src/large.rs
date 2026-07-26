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
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::os;
use crate::stats::Stats;

/// Mappings a heap may retain for reuse instead of unmapping. Sixteen
/// bounds idle retention to a few MB per shard; the reclaim tick drains
/// the pool, so retention beyond a tick needs sustained traffic — which
/// is exactly when it is earning its keep.
pub(crate) const POOL_SLOTS: usize = 16;

/// Bytes currently parked in retention pools, process-wide — reported
/// inside the `hysteresis` term ("retained rather than released", the
/// same policy the empty-span rule applies at span scale).
static POOLED: AtomicU64 = AtomicU64::new(0);

/// A per-heap pool of recently released direct mappings, keyed by exact
/// mapped length.
///
/// The syscall counter forced this: after the class table reached its
/// 64 KiB-span ceiling, the legacy shape still ran ~17k direct
/// allocations a second — dispatch and reply buffers growing through a
/// 36 KB–300 KB ladder — and each paid an mmap on birth and a munmap on
/// death while glibc paid zero syscalls
/// (finding `2026-07-27-mmap-lock-was-the-killer.md`, follow-up).
/// Growth ladders repeat the same page-rounded lengths deterministically,
/// so exact-length matching is both trivial and sufficient; a miss just
/// maps, and anything unusual falls straight through.
pub(crate) struct LargePool {
    entries: [(usize, usize); POOL_SLOTS], // (addr, mapped_len)
    len: u8,
}

impl LargePool {
    pub(crate) const fn new() -> Self {
        Self { entries: [(0, 0); POOL_SLOTS], len: 0 }
    }

    /// Take a parked mapping of exactly `mapped` bytes.
    fn take(&mut self, mapped: usize) -> Option<NonNull<u8>> {
        for i in 0..self.len as usize {
            if self.entries[i].1 == mapped {
                let (addr, _) = self.entries[i];
                self.len -= 1;
                self.entries[i] = self.entries[self.len as usize];
                POOLED.fetch_sub(mapped as u64, Relaxed);
                return NonNull::new(addr as *mut u8);
            }
        }
        None
    }

    /// Park a mapping; refuses when full (caller unmaps).
    fn park(&mut self, ptr: NonNull<u8>, mapped: usize) -> bool {
        if self.len as usize == POOL_SLOTS {
            return false;
        }
        self.entries[self.len as usize] = (ptr.as_ptr() as usize, mapped);
        self.len += 1;
        POOLED.fetch_add(mapped as u64, Relaxed);
        true
    }

    /// Unmap everything parked (the reclaim tick's job).
    pub(crate) fn drain(&mut self) {
        for i in 0..self.len as usize {
            let (addr, mapped) = self.entries[i];
            POOLED.fetch_sub(mapped as u64, Relaxed);
            counters::sub_mapped_only(mapped as u64);
            // SAFETY: parked mappings are live, exactly `mapped` bytes,
            // and referenced by nobody once off the pool.
            unsafe {
                os::unmap(NonNull::new_unchecked(addr as *mut u8), mapped);
            }
        }
        self.len = 0;
    }
}

/// The counters themselves. See the module docs for why they live here.
mod counters {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub(super) static MAPPED: AtomicU64 = AtomicU64::new(0);
    pub(super) static LIVE: AtomicU64 = AtomicU64::new(0);
    pub(super) static ROUNDING: AtomicU64 = AtomicU64::new(0);
    pub(super) static COUNT: AtomicU64 = AtomicU64::new(0);

    pub(super) fn add(mapped: u64, requested: u64) {
        MAPPED.fetch_add(mapped, Relaxed);
        add_live_only(mapped, requested);
    }

    /// A pooled reuse: the mapping was already counted, only its
    /// occupancy changes.
    pub(super) fn add_live_only(mapped: u64, requested: u64) {
        LIVE.fetch_add(requested, Relaxed);
        ROUNDING.fetch_add(mapped - requested, Relaxed);
        COUNT.fetch_add(1, Relaxed);
    }

    /// A park: occupancy ends, the mapping stays counted (it is still
    /// mapped — the pool holds it).
    pub(super) fn sub_live_only(mapped: u64, requested: u64) {
        LIVE.fetch_sub(requested, Relaxed);
        ROUNDING.fetch_sub(mapped - requested, Relaxed);
        COUNT.fetch_sub(1, Relaxed);
    }

    /// The pool released a mapping to the OS.
    pub(super) fn sub_mapped_only(mapped: u64) {
        MAPPED.fetch_sub(mapped, Relaxed);
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

/// Map `size` bytes directly, reusing a parked mapping when one of
/// exactly the right length is waiting. `None` when the OS refuses or
/// the alignment is stricter than a fresh mapping provides.
pub(crate) fn alloc(pool: &mut LargePool, size: usize, align: usize) -> Option<NonNull<u8>> {
    if align > os::PAGE {
        return None;
    }
    let mapped = os::round_up(size, os::PAGE);
    if let Some(p) = pool.take(mapped) {
        counters::add_live_only(mapped as u64, size as u64);
        return Some(p);
    }
    let p = os::map_aligned(mapped, os::PAGE)?;
    counters::add(mapped as u64, size as u64);
    Some(p)
}

/// # Safety
/// `ptr`/`size` must come from [`alloc`] and not be used afterwards.
pub(crate) unsafe fn dealloc(pool: &mut LargePool, ptr: NonNull<u8>, size: usize) {
    let mapped = os::round_up(size, os::PAGE);
    counters::sub_live_only(mapped as u64, size as u64);
    if pool.park(ptr, mapped) {
        return;
    }
    counters::sub_mapped_only(mapped as u64);
    // SAFETY: delegated to the caller's contract.
    unsafe { os::unmap(ptr, mapped) };
}
