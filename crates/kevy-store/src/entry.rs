//! [`Entry`] — the per-key record: value + packed TTL deadline. Its weight
//! and access clock live beside the table (see `entry_weight`), so a slot
//! is key 24 + entry 40 = 64 bytes, one cache line.

use crate::clock::{now_ns, pack_deadline};
use crate::value::Value;
use core::num::NonZeroU64;

/// Per-key entry, 40 bytes:
///
/// - `value`: 32 bytes (boxed-collection enum).
/// - `expire_at_ns`: `Option<NonZeroU64>` = ns since process start.
///   Niche optimisation makes this 8 bytes, not the 16 a bare
///   `Option<Instant>` would cost.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) value: Value,
    pub(crate) expire_at_ns: Option<NonZeroU64>,
}

impl Entry {
    /// Build a fresh entry. `deadline_ns` is an absolute monotonic deadline (ns since epoch), or
    /// `None` for a key that never expires.
    #[inline]
    pub(crate) fn new(value: Value, deadline_ns: Option<u64>) -> Self {
        Self { value, expire_at_ns: deadline_ns.and_then(pack_deadline) }
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

// Pin the slot at one cache line: a 24-byte key and a 40-byte entry.
const _: () = {
    assert!(core::mem::size_of::<Entry>() == 40);
    assert!(core::mem::size_of::<(crate::SmallBytes, Entry)>() == 64);
};
