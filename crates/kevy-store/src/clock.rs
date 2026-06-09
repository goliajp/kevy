//! Monotonic deadline encoding. Every `Entry::expire_at_ns` is a nanosecond
//! offset from a process-start [`epoch`], packed as `Option<NonZeroU64>` so the
//! niche optimisation keeps the field at 8 bytes. Split out of `lib.rs` to keep
//! it under the 500-LOC house cap.

use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Process-start anchor: every `Entry::expire_at_ns` is a nanosecond
/// offset from this `Instant`, encoded as `Option<NonZeroU64>` so the
/// niche optimisation lets the field cost 8 bytes (vs 16 for a bare
/// `Option<Instant>`). 584-year range from process start — Y2538-proof.
pub(crate) fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Encode an absolute `Instant` as ns-since-process-start. Returns `None`
/// when `t == epoch()` exactly (sentinel collision); in practice an entry
/// inserted at exactly t=0 from process start with TTL=0 is the only path
/// there, and TTL=0 isn't a valid expiry the API ever takes.
#[inline]
pub(crate) fn pack_deadline(t: Instant) -> Option<NonZeroU64> {
    let ns = t.saturating_duration_since(epoch()).as_nanos() as u64;
    NonZeroU64::new(ns)
}

/// Decode a packed deadline back into an `Instant` for the rare paths
/// (`pttl`, snapshot dump) that need real-clock math.
#[inline]
pub(crate) fn unpack_deadline(ns: NonZeroU64) -> Instant {
    epoch() + Duration::from_nanos(ns.get())
}

/// Current monotonic time as ns-since-[`epoch`] — the unit `Entry::expire_at_ns`
/// is stored in and `Store::cached_ns` caches. Reads `Instant::now()` once.
#[inline]
pub(crate) fn now_ns() -> u64 {
    Instant::now().saturating_duration_since(epoch()).as_nanos() as u64
}
