//! Active TTL reaper — Redis's `activeExpireCycle`, adapted to the
//! thread-per-core / single-shard `Store`.
//!
//! Lazy expiry (in `live_entry[_mut]`) still handles the common case where
//! the next access to a TTL'd key removes it. The active reaper exists for
//! the harder case: a key has TTL but is never touched again, so without an
//! explicit sweep it would sit in the map until the next FLUSH or eviction.
//!
//! Entry point: [`Store::tick_expire`]. The shard runtime calls it at the
//! configured `[expiry].hz` cadence (default 10 Hz / every 100 ms);
//! embedded users without a runtime call it themselves from whatever event
//! loop they have (mandatory for WASM, which has no threads).

use crate::Store;
use std::time::Instant;

/// What [`Store::tick_expire`] saw and did. Surfaced for tests, INFO
/// keyspace, and (eventually) Wave 2 task #4's crash-safe verifier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpireStats {
    /// Total TTL-bearing keys sampled across all rounds.
    pub sampled: u32,
    /// How many of those were past their deadline and got removed.
    pub expired: u32,
    /// Rounds executed before the loop exited (either `max_rounds` reached
    /// or in-batch expire-rate dropped below the continuation threshold).
    pub rounds: u32,
}

/// Continuation threshold: when an in-batch expire-rate is above this
/// percentage, run another round (the keyspace is "expiry-heavy"). Mirrors
/// Redis's 25% from `activeExpireCycle`.
const EXPIRE_RATE_CONTINUATION: u32 = 25;

/// Sample a single round of up to `samples` TTL-bearing keys starting at a
/// random bucket; remove any that are past their deadline. Returns
/// `(sampled, expired)` counts for this round. Walking is `O(visited)` —
/// bounded by `2 * map.capacity()` to keep a sparsely-populated table from
/// spinning the inner scan forever.
pub(crate) fn sample_round(
    store: &mut Store,
    samples: usize,
    now: Instant,
) -> (u32, u32) {
    let cap = store.map.capacity();
    if cap == 0 || store.map.is_empty() {
        return (0, 0);
    }
    // Random start derived from the access-ordinal clock; Fibonacci-hash
    // multiplier shifts the sampling window every call so we don't re-visit
    // the same bucket range twice in a row. (No-quality PRNG needed for
    // sampling, just want to spread starting positions.)
    store.clock_counter = store.clock_counter.wrapping_add(1);
    let start = (store
        .clock_counter
        .wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize)
        % cap;
    let mut victims: Vec<Vec<u8>> = Vec::with_capacity(samples);
    let mut sampled = 0u32;
    // Single-pass walk from `start` to end. Sparse tables / unlucky starts
    // may sample fewer than `samples` per round; that's fine — the next
    // tick picks a different random start, so over time the keyspace is
    // covered uniformly. Avoids the double-count a `chain(wrap)` would
    // create when the same bucket range is visited twice.
    {
        for (k, e) in store.map.iter_from_bucket(start) {
            if sampled as usize >= samples {
                break;
            }
            let Some(deadline_ns) = e.expire_at_ns else {
                continue;
            };
            sampled += 1;
            if crate::unpack_deadline(deadline_ns) <= now {
                victims.push(k.to_vec());
            }
        }
    }
    let expired = victims.len() as u32;
    for k in &victims {
        store.remove_entry(k);
    }
    // Active-expire-driven removals are still expirations from the shard's
    // perspective — surface them under the same counter `MEMORY STATS` /
    // `INFO memory` already exposes.
    if expired > 0 {
        store.expired_keys_total = store
            .expired_keys_total
            .saturating_add(expired as u64);
    }
    let _ = sampled; // silence unused warning if all returned early
    (sampled, expired)
}

impl Store {
    /// Run up to `max_rounds` of active-expiry sampling against this shard.
    ///
    /// Per round: sample `samples_per_round` TTL-bearing keys at random and
    /// drop any whose deadline has passed. Stop early as soon as the
    /// in-batch expire-rate drops below 25 % (Redis's `activeExpireCycle`
    /// continuation threshold) — that's the signal the keyspace doesn't
    /// have a "thick band" of expired keys to clean up right now.
    ///
    /// Cost when there are no TTL-bearing keys at all: one map-emptiness
    /// check + a single bucket-iter probe per round. Designed so the active
    /// reaper is never a tax on TTL-free workloads.
    pub fn tick_expire(&mut self, samples_per_round: usize, max_rounds: u32) -> ExpireStats {
        // Refresh the coarse cached clock every tick (the read path's lazy
        // expiry compares against it) — even when there's nothing to reap.
        self.refresh_clock();
        if samples_per_round == 0 || max_rounds == 0 || self.map.is_empty() {
            return ExpireStats::default();
        }
        let now = Instant::now();
        let mut total_sampled = 0u32;
        let mut total_expired = 0u32;
        let mut rounds = 0u32;
        // Single-pass sample_round can return sampled=0 when the random
        // start lands in an empty bucket region (sparse tables / unlucky
        // starts). Allow 3 consecutive zero-sample rounds before declaring
        // the keyspace TTL-free this tick, so a small table doesn't miss
        // its expired keys for several ticks.
        let mut consecutive_zero = 0u32;
        for _ in 0..max_rounds {
            let (sampled, expired) = sample_round(self, samples_per_round, now);
            rounds += 1;
            total_sampled = total_sampled.saturating_add(sampled);
            total_expired = total_expired.saturating_add(expired);
            if sampled == 0 {
                consecutive_zero += 1;
                if consecutive_zero >= 3 {
                    break;
                }
                continue;
            }
            consecutive_zero = 0;
            // Continuation gate: only push another round if THIS round was
            // expiry-heavy. A round that finds nothing expired-enough exits.
            if expired * 100 < sampled * EXPIRE_RATE_CONTINUATION {
                break;
            }
        }
        ExpireStats {
            sampled: total_sampled,
            expired: total_expired,
            rounds,
        }
    }

    /// Total keys expired (by lazy reap OR active reaper). Surfaced via
    /// `INFO keyspace` and `MEMORY STATS` once those grow the field.
    #[inline]
    pub fn expired_keys_total(&self) -> u64 {
        self.expired_keys_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::SmallBytes;
    use std::time::Duration;

    #[test]
    fn tick_expire_drops_past_deadline() {
        let mut s = Store::new();
        s.set(b"k1", b"v".to_vec(), Some(Duration::from_millis(1)), false, false);
        s.set(b"k2", b"v".to_vec(), Some(Duration::from_millis(1)), false, false);
        s.set(b"perm", b"v".to_vec(), None, false, false);
        std::thread::sleep(Duration::from_millis(20));
        let stats = s.tick_expire(20, 16);
        assert_eq!(stats.expired, 2, "both TTL'd keys should be reaped");
        assert!(stats.sampled >= 2);
        assert_eq!(s.dbsize(), 1, "perm survives");
        assert!(s.expired_keys_total() >= 2);
    }

    #[test]
    fn tick_expire_no_op_on_fresh_ttls() {
        let mut s = Store::new();
        s.set(b"k1", b"v".to_vec(), Some(Duration::from_secs(3600)), false, false);
        s.set(b"k2", b"v".to_vec(), Some(Duration::from_secs(3600)), false, false);
        let stats = s.tick_expire(20, 16);
        assert_eq!(stats.expired, 0, "no fresh TTL should expire");
        // sampled may be 0..=2 depending on how many our walk hit
        assert_eq!(s.dbsize(), 2);
    }

    #[test]
    fn tick_expire_no_op_on_ttl_free_keyspace() {
        let mut s = Store::new();
        for i in 0..50 {
            s.set(format!("k{i}").as_bytes(), b"v".to_vec(), None, false, false);
        }
        let stats = s.tick_expire(20, 16);
        assert_eq!(stats.expired, 0);
        assert_eq!(stats.sampled, 0, "no TTL'd keys ⇒ nothing sampled");
        // Loop tolerates up to 3 consecutive zero-sample rounds (the
        // unlucky-start guard) before exiting, so a TTL-free keyspace
        // costs at most 3 cheap bucket-iter passes per tick.
        assert!(stats.rounds <= 3, "got {}", stats.rounds);
    }

    #[test]
    fn tick_expire_zero_args_short_circuit() {
        let mut s = Store::new();
        s.set(b"k", b"v".to_vec(), Some(Duration::from_millis(1)), false, false);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(s.tick_expire(0, 16), ExpireStats::default());
        assert_eq!(s.tick_expire(20, 0), ExpireStats::default());
        // store still has the expired key (active reaper disabled).
        assert_eq!(s.dbsize(), 1);
    }

    #[test]
    fn tick_expire_loops_on_heavy_batch() {
        let mut s = Store::new();
        // 40 TTL'd keys (all expired) + 1 perm. A single tick samples from
        // a random bucket window, so we may need several ticks for full
        // coverage of a 40-key keyspace — that matches how `activeExpire`
        // converges in production (10 ticks/sec until everything's cleaned).
        for i in 0..40 {
            s.set(
                format!("k{i}").as_bytes(),
                b"v".to_vec(),
                Some(Duration::from_millis(1)),
                false,
                false,
            );
        }
        s.set(b"perm", b"v".to_vec(), None, false, false);
        std::thread::sleep(Duration::from_millis(20));
        let mut total_expired = 0u32;
        let mut any_round_ge_2 = false;
        for _ in 0..20 {
            let stats = s.tick_expire(20, 16);
            total_expired += stats.expired;
            if stats.rounds >= 2 {
                any_round_ge_2 = true;
            }
            if s.dbsize() == 1 {
                break;
            }
        }
        assert_eq!(total_expired, 40);
        assert!(any_round_ge_2, "at least one heavy-batch tick should loop");
        assert_eq!(s.dbsize(), 1);
        let _ = SmallBytes::from_slice(b"k0"); // touch SmallBytes import
    }
}
