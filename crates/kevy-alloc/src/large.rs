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

/// Mappings retained for reuse instead of unmapping, process-wide.
/// Thirty-two bounds idle retention to a few MB total; every shard's
/// reclaim tick drains it, so retention beyond a tick needs sustained
/// traffic — which is exactly when it is earning its keep.
pub(crate) const POOL_SLOTS: usize = 32;

/// Bytes currently parked in retention pools, process-wide — reported
/// inside the `hysteresis` term ("retained rather than released", the
/// same policy the empty-span rule applies at span scale).
static POOLED: AtomicU64 = AtomicU64::new(0);

/// The retention pool, keyed by exact mapped length — and process-wide,
/// which the syscall counter decided, not taste.
///
/// Its existence: after the class table reached its 64 KiB-span
/// ceiling, the legacy shape still ran ~17k direct allocations a second
/// — dispatch and reply buffers growing through a 36 KB–300 KB ladder —
/// each paying an mmap on birth and a munmap on death while glibc paid
/// zero syscalls (finding `2026-07-27-mmap-lock-was-the-killer.md`,
/// follow-up).
///
/// Its scope: the first version was per-heap and moved the count by
/// **nothing**, because these buffers are born on one shard and die on
/// another — the freeing side's pool filled once and stayed useless
/// while the allocating side's stayed empty. A large mapping has no
/// segment, so no owner is recoverable from its address and no route
/// home exists; the pool must be shared. One spinlock guards it: at
/// ~17k operations a second against 8.5M ops served this is a cold
/// path, and an uncontended spinlock costs two orders of magnitude less
/// than the syscall it replaces.
///
/// Growth ladders repeat the same page-rounded lengths
/// deterministically, so exact-length matching is both trivial and
/// sufficient; a miss just maps, and anything unusual falls through.
struct PoolInner {
    entries: [(usize, usize, u8); POOL_SLOTS], // (addr, mapped_len, parked_gen)
    len: u8,
    /// Drain-call generation. Ages are measured in drain calls — one
    /// per shard per tick — because the pool is process-wide and has
    /// no tick of its own. With N shards the wall-clock bound is
    /// POOL_AGE_DRAINS / N ticks; the bound existing at all is what
    /// matters (the no-reclaim wedge's lesson), not its exact length.
    generation: u8,
}

static POOL_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static mut POOL: PoolInner =
    PoolInner { entries: [(0, 0, 0); POOL_SLOTS], len: 0, generation: 0 };

/// Run `f` holding the pool lock.
fn with_pool<R>(f: impl FnOnce(&mut PoolInner) -> R) -> R {
    use core::sync::atomic::Ordering;
    while POOL_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: the spinlock gives exclusive access for `f`'s duration,
    // and `f` cannot re-enter — nothing inside it allocates.
    let out = f(unsafe { &mut *core::ptr::addr_of_mut!(POOL) });
    POOL_LOCK.store(false, Ordering::Release);
    out
}

/// Take a parked mapping of exactly `mapped` bytes.
fn pool_take(mapped: usize) -> Option<NonNull<u8>> {
    with_pool(|p| {
        for i in 0..p.len as usize {
            if p.entries[i].1 == mapped {
                let (addr, _, _) = p.entries[i];
                p.len -= 1;
                p.entries[i] = p.entries[p.len as usize];
                POOLED.fetch_sub(mapped as u64, Relaxed);
                return NonNull::new(addr as *mut u8);
            }
        }
        None
    })
}

/// Park a mapping; refuses when full (the caller unmaps).
fn pool_park(ptr: NonNull<u8>, mapped: usize) -> bool {
    with_pool(|p| {
        if p.len as usize == POOL_SLOTS {
            return false;
        }
        p.entries[p.len as usize] = (ptr.as_ptr() as usize, mapped, p.generation);
        p.len += 1;
        POOLED.fetch_add(mapped as u64, Relaxed);
        true
    })
}

/// Unmap parked mappings that have aged past the pacing bound; young
/// entries stay parked so a burst cycle re-takes them instead of
/// paying mmap + kernel zero-fill again
/// (RFC 2026-08-06-v5-reclaim-pacing). Ages are in drain calls and
/// every entry still leaves within POOL_AGE_DRAINS of them — the same
/// unconditional liveness bound as the span sweep's.
pub(crate) fn pool_drain() {
    /// Drain calls an entry may sit parked. With N shards each
    /// ticking, wall-clock retention is this / N ticks.
    const POOL_AGE_DRAINS: u8 = 64;
    // Collected under the lock, unmapped outside it: munmap takes the
    // process mmap_lock, and holding a spinlock across that invites
    // exactly the convoy this pool exists to prevent.
    let mut held: [(usize, usize); POOL_SLOTS] = [(0, 0); POOL_SLOTS];
    let n = with_pool(|p| {
        p.generation = p.generation.wrapping_add(1);
        let mut n = 0usize;
        let mut i = 0usize;
        while i < p.len as usize {
            let (addr, mapped, born) = p.entries[i];
            if p.generation.wrapping_sub(born) >= POOL_AGE_DRAINS {
                held[n] = (addr, mapped);
                n += 1;
                p.len -= 1;
                p.entries[i] = p.entries[p.len as usize];
            } else {
                i += 1;
            }
        }
        n
    });
    for &(addr, mapped) in &held[..n] {
        POOLED.fetch_sub(mapped as u64, Relaxed);
        counters::sub_mapped_only(mapped as u64);
        // SAFETY: parked mappings are live, exactly `mapped` bytes, and
        // referenced by nobody once off the pool.
        unsafe {
            os::unmap(NonNull::new_unchecked(addr as *mut u8), mapped);
        }
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
        // Parked mappings: retained-rather-than-released, the same
        // policy the empty-span rule applies at span scale, so the same
        // term prices them (contract §1, widened with this reason).
        hysteresis: POOLED.load(Relaxed),
        large_count: counters::COUNT.load(Relaxed),
        ..Stats::default()
    }
}

/// Map `size` bytes directly, reusing a parked mapping when one of
/// exactly the right length is waiting. `None` when the OS refuses or
/// the alignment is stricter than a fresh mapping provides.
pub(crate) fn alloc(size: usize, align: usize) -> Option<NonNull<u8>> {
    if align > os::PAGE {
        return None;
    }
    let mapped = os::round_up(size, os::PAGE);
    if let Some(p) = pool_take(mapped) {
        counters::add_live_only(mapped as u64, size as u64);
        return Some(p);
    }
    let p = os::map_aligned(mapped, os::PAGE)?;
    counters::add(mapped as u64, size as u64);
    Some(p)
}

/// # Safety
/// `ptr`/`size` must come from [`alloc`] and not be used afterwards.
pub(crate) unsafe fn dealloc(ptr: NonNull<u8>, size: usize) {
    let mapped = os::round_up(size, os::PAGE);
    counters::sub_live_only(mapped as u64, size as u64);
    if pool_park(ptr, mapped) {
        return;
    }
    counters::sub_mapped_only(mapped as u64);
    // SAFETY: delegated to the caller's contract.
    unsafe { os::unmap(ptr, mapped) };
}
