//! Maxmemory enforcement and the 8 eviction policies.
//!
//! Activated by [`Store::set_max_memory`]. Hot-path entry points:
//!
//! - [`touch_on_access`] — bump the per-entry LRU ordinal / LFU counter on
//!   reads and writes (called only when `maxmemory > 0`).
//! - [`evict_until_under_limit`] — sample-based eviction loop run after a
//!   write that pushed `used_memory` above the configured ceiling.
//!
//! All policies operate on N random samples per round (Redis-style; default
//! `maxmemory-samples = 5`). Random samples avoid the O(`map.len()`) cost of
//! a "true" global LRU/LFU pick at the price of mild approximation — the
//! exact same trade-off Redis ships with.

use crate::{Entry, EvictionPolicy, Store};
use std::time::Instant;

/// `maxmemory-samples` Redis default. Each `evict_one` picks the worst out
/// of this many randomly-sampled keys.
const N_SAMPLES: usize = 5;

/// Evict until `used_memory ≤ maxmemory * 19/20` (5% headroom). Without
/// headroom each subsequent write would re-enter eviction; with headroom we
/// amortise the sampling cost across many writes.
const HEADROOM_NUM: u64 = 19;
const HEADROOM_DEN: u64 = 20;

/// Hard safety cap to prevent a pathological loop from blocking the reactor
/// indefinitely (e.g. if every sampled key is somehow un-evictable). Reached
/// only on misconfiguration — under normal loads a single `evict_until_*`
/// call drops dozens-to-thousands of keys.
const MAX_EVICTIONS_PER_CALL: usize = 1_000_000;

/// Per-LFU-counter initial value (matches Redis's `LFU_INIT_VAL`). Gives
/// brand-new keys some headroom before they're considered eviction candidates.
const LFU_INIT_VAL: u8 = 5;

/// Log-scale increment denominator: `p(increment) = 1 / (baseval * factor + 1)`
/// where `baseval = counter - LFU_INIT_VAL`. Matches Redis's
/// `lfu-log-factor = 10` default.
const LFU_LOG_FACTOR: u32 = 10;

/// Maximum effective counter value (`MAX_LFU_COUNTER` in Redis is 255 too).
const LFU_COUNTER_MAX: u8 = 255;

/// Per-policy access touch. Cheap when policy is Random/TTL/NoEviction
/// (single branch, no memory write); writes the LRU clock or steps the LFU
/// counter otherwise.
#[inline]
pub(crate) fn touch_on_access(e: &mut Entry, policy: EvictionPolicy, clock: u32) {
    if policy.uses_lru() {
        e.lru_clock = clock;
    } else if policy.uses_lfu() {
        let counter = e.lru_clock as u8;
        let next = lfu_log_incr(counter, clock);
        e.lru_clock = (e.lru_clock & 0xFFFF_FF00) | next as u32;
    }
}

/// Probabilistically increment the LFU counter (saturating at 255). Returns
/// the new counter. Uses a splitmix-style hash of `clock` as the PRNG so we
/// don't drag a stateful RNG through the hot path.
#[inline]
fn lfu_log_incr(counter: u8, clock: u32) -> u8 {
    if counter == LFU_COUNTER_MAX {
        return counter;
    }
    let baseval = counter.saturating_sub(LFU_INIT_VAL) as u32;
    let threshold = baseval.saturating_mul(LFU_LOG_FACTOR).saturating_add(1);
    if threshold == 1 || splitmix32(clock) % threshold == 0 {
        counter.saturating_add(1)
    } else {
        counter
    }
}

/// Splitmix32 — a small PRNG that turns a monotonic clock into a usably
/// random u32. Used for LFU probabilistic increment and for picking sample
/// starting positions. Not crypto-grade; we don't need it to be.
#[inline]
fn splitmix32(x: u32) -> u32 {
    let mut z = x.wrapping_add(0x9E37_79B9);
    z = (z ^ (z >> 16)).wrapping_mul(0x85EB_CA6B);
    z = (z ^ (z >> 13)).wrapping_mul(0xC2B2_AE35);
    z ^ (z >> 16)
}

/// Run the eviction loop until `used_memory ≤ maxmemory * 19/20`, or until
/// no eligible candidate remains, or until [`MAX_EVICTIONS_PER_CALL`] keys
/// have been dropped. Returns the number of keys evicted.
pub(crate) fn evict_until_under_limit(store: &mut Store) -> usize {
    if store.eviction_policy == EvictionPolicy::NoEviction {
        // Honoured at `precheck_for_write` — if we got here under NoEviction
        // we have no way to free memory; leave it as-is. The next write's
        // precheck will refuse and return OOM to the client.
        return 0;
    }
    let target = store
        .maxmemory
        .saturating_mul(HEADROOM_NUM)
        / HEADROOM_DEN;
    let mut evicted = 0;
    let mut consecutive_no_candidate = 0;
    while store.used_memory > target {
        if evict_one(store) {
            evicted += 1;
            consecutive_no_candidate = 0;
        } else {
            consecutive_no_candidate += 1;
            if consecutive_no_candidate >= 3 {
                break;
            }
        }
        if evicted >= MAX_EVICTIONS_PER_CALL {
            break;
        }
    }
    evicted
}

/// Sample [`N_SAMPLES`] random keys, pick the worst per policy, remove it.
/// Returns `true` if a key was evicted, `false` if no eligible candidate was
/// found (e.g. volatile-* policy on a TTL-free keyspace).
fn evict_one(store: &mut Store) -> bool {
    let Some(victim) = sample_and_pick(store, N_SAMPLES) else {
        return false;
    };
    if store.remove_entry(&victim).is_some() {
        store.evictions_total += 1;
        true
    } else {
        // Map state changed between sampling and removal (race-impossible in
        // single-threaded shard, but be defensive).
        false
    }
}

/// Pick the worst key from `n` random samples per the active policy. Returns
/// `None` when no eligible candidate exists (empty map, or volatile-* with
/// zero TTL-bearing keys).
fn sample_and_pick(store: &mut Store, n: usize) -> Option<Vec<u8>> {
    let cap = store.map.capacity();
    if cap == 0 || store.map.is_empty() {
        return None;
    }
    let policy = store.eviction_policy;
    let volatile_only = policy.is_volatile();
    let now = Instant::now();
    let clock = store.clock_counter as u32;
    // Random start derived from the access ordinal — every call shifts so we
    // don't sample the same bucket window twice in a row.
    let start = (splitmix32(clock) as usize) % cap;

    let mut best: Option<(Vec<u8>, i64)> = None;
    let mut taken = 0;
    let mut visited = 0;
    // Walk the bucket ring beginning at `start`; cap the linear scan so a
    // sparsely-populated table doesn't spin forever.
    let visit_cap = cap.saturating_mul(2);
    let primary = store.map.iter_from_bucket(start);
    let wrap = store.map.iter_from_bucket(0);
    for (k, e) in primary.chain(wrap) {
        if visited >= visit_cap {
            break;
        }
        visited += 1;
        if volatile_only && e.expire_at.is_none() {
            continue;
        }
        let score = score_entry(e, policy, now, clock);
        if best.as_ref().is_none_or(|(_, bs)| score < *bs) {
            best = Some((k.to_vec(), score));
        }
        taken += 1;
        if taken >= n {
            break;
        }
    }
    best.map(|(k, _)| k)
}

/// Per-policy "badness" score. Lower = more evictable. Returned as `i64` so
/// callers can compare directly; we never negate-overflow because none of
/// the inputs reach i64::MIN.
#[inline]
fn score_entry(e: &Entry, policy: EvictionPolicy, now: Instant, clock: u32) -> i64 {
    match policy {
        EvictionPolicy::AllKeysLru | EvictionPolicy::VolatileLru => e.lru_clock as i64,
        EvictionPolicy::AllKeysLfu | EvictionPolicy::VolatileLfu => {
            (e.lru_clock & 0xFF) as i64
        }
        EvictionPolicy::AllKeysRandom | EvictionPolicy::VolatileRandom => {
            // Stamp each sampled entry with a fresh splitmix bit so the
            // "lowest score" rule picks uniformly at random.
            splitmix32(clock ^ e.lru_clock) as i64
        }
        EvictionPolicy::VolatileTtl => match e.expire_at {
            Some(t) => t.saturating_duration_since(now).as_millis() as i64,
            None => i64::MAX, // unreachable under volatile_only, but safe
        },
        EvictionPolicy::NoEviction => i64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lfu_increment_eventually_saturates() {
        let mut c = LFU_INIT_VAL;
        // 1M synthetic accesses; counter should approach (but never exceed) 255.
        for clock in 0..1_000_000u32 {
            c = lfu_log_incr(c, clock);
        }
        assert!(c <= LFU_COUNTER_MAX);
        assert!(c >= LFU_INIT_VAL + 5, "expected meaningful growth, got {c}");
    }

    #[test]
    fn lfu_increment_is_log_scaled() {
        // Counter near LFU_INIT_VAL increments often; counter near MAX rarely.
        let mut low_hits = 0u32;
        let mut high_hits = 0u32;
        for clock in 0..10_000u32 {
            if lfu_log_incr(LFU_INIT_VAL, clock) > LFU_INIT_VAL {
                low_hits += 1;
            }
            if lfu_log_incr(200, clock) > 200 {
                high_hits += 1;
            }
        }
        assert!(low_hits > 100 * high_hits,
            "log scaling broken: low={low_hits} high={high_hits}");
    }

    #[test]
    fn splitmix_avalanches_neighbours() {
        // Adjacent inputs should produce wildly different outputs (this is the
        // whole reason we don't just use the raw clock as a "random" value).
        let a = splitmix32(0);
        let b = splitmix32(1);
        let diff = (a ^ b).count_ones();
        assert!(diff > 10, "splitmix didn't avalanche: a={a:x} b={b:x}");
    }
}
