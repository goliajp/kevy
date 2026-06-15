//! [`Entry`] — the per-key record: value + packed TTL deadline + cached
//! weight + LRU/LFU clock. Split from `lib.rs` to keep it under the
//! 500-LOC house rule.

use crate::clock::{now_ns, pack_deadline};
use crate::value::Value;
use std::num::NonZeroU64;

/// Per-entry weight ceiling — the field is `u32` so accounting saturates
/// at 4 GiB per entry. Real-world Redis values are well below this; the
/// ceiling only matters when a single hash / list / zset exceeds 4 GiB,
/// in which case `MEMORY USAGE` and the maxmemory accounting under-
/// report that one entry by the overflow amount. Acceptable v1.0 tradeoff
/// — keeps `Entry` at 48 bytes (vs 56 if we kept `u64`).
pub(crate) const WEIGHT_MAX: u32 = u32::MAX;

/// Per-key entry — packed to 48 bytes (vs 64 in the original
/// `Value + Option<Instant> + u64 weight + u32 clock + 4 pad` layout):
///
/// - `value`: 32 bytes (boxed-collection enum).
/// - `expire_at_ns`: `Option<NonZeroU64>` = ns since process start.
///   Niche optimisation makes this 8 bytes, not the 16 a bare
///   `Option<Instant>` would cost.
/// - `weight`: `u32`. Cached `key.heap_bytes() + value.weight()` for
///   O(1) eviction & `MEMORY USAGE`. Saturates at 4 GiB per entry.
/// - `lru_clock`: `u32`. LRU = monotonic op counter; LFU = packed
///   `[16-bit decay-tick | 8-bit log-counter]`. Only updated when
///   `Store::maxmemory > 0`.
///
/// Storage saving over the original layout: 16 bytes per entry = 25 %.
/// For a 1 M-key shard that's ~16 MB of RSS back.
pub(crate) struct Entry {
    pub(crate) value: Value,
    pub(crate) expire_at_ns: Option<NonZeroU64>,
    pub(crate) weight: u32,
    pub(crate) lru_clock: u32,
}

impl Entry {
    /// Build a fresh entry with weight + lru_clock uninitialised (the
    /// caller — usually [`Store::insert_entry`] — will compute and stamp them).
    /// `deadline_ns` is an absolute monotonic deadline (ns since epoch), or
    /// `None` for a key that never expires.
    #[inline]
    pub(crate) fn new(value: Value, deadline_ns: Option<u64>) -> Self {
        Self {
            value,
            expire_at_ns: deadline_ns.and_then(pack_deadline),
            weight: 0,
            lru_clock: 0,
        }
    }

    /// Cached entry weight as a `u64` for arithmetic uniformity with the
    /// `Store::used_memory: u64` accumulator. Zero-cost cast.
    #[inline]
    pub(crate) fn weight(&self) -> u64 {
        self.weight as u64
    }

    /// LRU / LFU clock value (eviction-only).
    #[inline]
    pub(crate) fn lru_clock(&self) -> u32 {
        self.lru_clock
    }

    /// Overwrite the cached weight, saturating at the 4 GiB ceiling.
    #[inline]
    pub(crate) fn set_weight(&mut self, w: u64) {
        self.weight = w.min(WEIGHT_MAX as u64) as u32;
    }

    /// Overwrite the LRU/LFU clock field.
    #[inline]
    pub(crate) fn set_lru_clock(&mut self, c: u32) {
        self.lru_clock = c;
    }

    /// Apply a signed delta to the cached weight (saturating both directions).
    #[inline]
    pub(crate) fn add_to_weight(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
        let cur = self.weight as u64;
        let new = if delta >= 0 {
            cur.saturating_add(delta as u64)
        } else {
            cur.saturating_sub((-delta) as u64)
        };
        self.weight = new.min(WEIGHT_MAX as u64) as u32;
    }

    /// Is the entry past its deadline as of `now` (ns since epoch)? `None`
    /// deadline = never. Combines the two-step compare into one branch on the
    /// niche-optimised `Option`.
    #[inline]
    pub(crate) fn is_expired_at(&self, now: u64) -> bool {
        match self.expire_at_ns {
            None => false,
            Some(ns) => ns.get() <= now,
        }
    }

    /// Lazy-expiry check for the per-access read path. A no-TTL key (the common
    /// case) short-circuits without reading any clock (the [`live_entry`] win).
    /// A TTL'd key compares its deadline against either the coarse cached clock
    /// (`use_cached` — when a reactor/reaper refreshes it, the Redis cached-
    /// `mstime` model, no per-get syscall) or a fresh `Instant::now()` (manual
    /// mode, where nothing else advances the clock so each get must read it).
    #[inline]
    pub(crate) fn is_expired(&self, use_cached: bool, cached_ns: u64) -> bool {
        match self.expire_at_ns {
            None => false,
            Some(d) => d.get() <= if use_cached { cached_ns } else { now_ns() },
        }
    }
}

// Pin the Entry layout: 32 (Value) + 8 (expire_at_ns, niche-opt) + 8 (packed)
// = 48 bytes. Any padding regression (e.g. someone re-adding a 4-byte field
// without packing) is caught at compile time.
const _: () = {
    assert!(std::mem::size_of::<Entry>() == 48);
};
