//! Monotonic deadline clock. The store measures time as `u64` nanoseconds
//! since a process-start epoch; every `Entry::expire_at_ns` is a deadline in
//! that unit, packed as `Option<NonZeroU64>` so the niche keeps the field at
//! 8 bytes (vs 16 for a bare `Option`).
//!
//! The clock *source* is cfg-gated:
//!
//! - **native** (everything except `wasm32-unknown-unknown`): the OS monotonic
//!   clock via [`std::time::Instant`]. Byte-for-byte the original hot path —
//!   zero added overhead.
//! - **`wasm32-unknown-unknown`**: that target has no `Instant` (calling
//!   `Instant::now()` traps `unreachable`), so the embedding host feeds time
//!   into an `AtomicU64` via [`set_clock_ns`] and `now_ns` just reads it. Until
//!   the host feeds a value the clock reads `0` (epoch) → keys look live and
//!   never expire early, the safe direction.
//!
//! Split out of `lib.rs` to keep it under the 500-LOC house cap.

use std::num::NonZeroU64;
use std::time::Duration;

/// Encode an absolute deadline (ns since epoch) as a packed `Option`. `None`
/// only when `deadline_ns == 0` — the niche sentinel, which a real TTL'd key
/// never lands on (it would require a deadline exactly at process start).
#[inline]
pub(crate) fn pack_deadline(deadline_ns: u64) -> Option<NonZeroU64> {
    NonZeroU64::new(deadline_ns)
}

/// Absolute deadline `ttl` after `now` (both ns since epoch). Saturates
/// rather than wrapping on an absurd TTL.
#[inline]
pub(crate) fn deadline_at(now_ns: u64, ttl: Duration) -> u64 {
    now_ns.saturating_add(ttl.as_nanos().min(u128::from(u64::MAX)) as u64)
}

/// Whole millis remaining from `now_ns` to a packed `deadline` (`0` once the
/// deadline is reached). Replaces the old `Instant`-based
/// `unpack().saturating_duration_since(now).as_millis()` with plain ns math.
#[inline]
pub(crate) fn remaining_ms(deadline: NonZeroU64, now_ns: u64) -> u64 {
    deadline.get().saturating_sub(now_ns) / 1_000_000
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
mod source {
    use std::sync::OnceLock;
    use std::time::Instant;

    /// Process-start anchor: `now_ns` measures ns since this instant. The
    /// 584-year `u64`-ns range from process start is Y2538-proof.
    fn epoch() -> Instant {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        *EPOCH.get_or_init(Instant::now)
    }

    /// Monotonic ns since [`epoch`] — one `Instant::now()` read.
    #[inline]
    pub(crate) fn now_ns() -> u64 {
        Instant::now().saturating_duration_since(epoch()).as_nanos() as u64
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod source {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Host-fed monotonic clock. `wasm32-unknown-unknown` has no `Instant`,
    /// so the embedding (browser / JS) advances this via [`set_clock_ns`].
    static MONO_NS: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub(crate) fn now_ns() -> u64 {
        MONO_NS.load(Ordering::Relaxed)
    }

    /// Feed the monotonic clock: `ns` is nanoseconds since an arbitrary fixed
    /// epoch (e.g. `Date.now() * 1e6`). The host should call this with a
    /// non-decreasing value before TTL-sensitive operations and once per
    /// reaper tick. A regression would make deadlines compare wrong, so keep
    /// it monotonic.
    #[inline]
    pub fn set_clock_ns(ns: u64) {
        MONO_NS.store(ns, Ordering::Relaxed);
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod wall {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Host-fed wall clock (Unix-epoch millis) for `now_unix_ms` —
    /// `SystemTime::now()` also traps on `wasm32-unknown-unknown`. Used by
    /// `XADD` auto-IDs and `EXPIREAT`/`PEXPIREAT`. Reads `0` until fed.
    static WALL_MS: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub(crate) fn now_unix_ms() -> u64 {
        WALL_MS.load(Ordering::Relaxed)
    }

    /// Feed the wall clock (Unix-epoch millis, e.g. `Date.now()`).
    #[inline]
    pub fn set_wall_clock_ms(ms: u64) {
        WALL_MS.store(ms, Ordering::Relaxed);
    }
}

pub(crate) use source::now_ns;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use source::set_clock_ns;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) use wall::now_unix_ms as wall_now_unix_ms;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use wall::set_wall_clock_ms;
