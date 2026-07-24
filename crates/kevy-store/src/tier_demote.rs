//! Tiering demotion/promotion primitives + the eviction fork (T3,
//! RFC §1 D2): the budgeted spill loop, victim sampling (reusing the
//! eviction sampler's scoring), in-place swap primitives, and the
//! compaction trigger. Compiled with the tier backend only (std,
//! off-wasm); the disabled builds' no-op twins live in `tier.rs`.

#![cfg(all(feature = "std", not(target_arch = "wasm32")))]

use kevy_vlog::{CompactOwner, VlogRef};

use crate::value::{ColdRef, Value};
use crate::{Entry, SmallBytes, Store, key_heap_bytes_for, tier_codec};

/// RFC §7: 32 records per demotion call, continuation on the shard tick.
const SPILL_BATCH: usize = 32;
/// RFC §7: values at or below inline size never spill.
const MIN_SPILL_BYTES: u64 = 64;
/// Demote watermark headroom (same 19/20 shape as eviction).
const WATERMARK_NUM: u64 = 19;
const WATERMARK_DEN: u64 = 20;
/// Compact a sealed file once its live ratio falls below this percent.
const COMPACT_LIVE_PCT: u32 = 50;

/// Is this value in the v1 spillable CLASS (ArcBulk / heap Hash /
/// inline hash)? Threshold and Cold-skip are the sampler's job.
#[inline]
fn spillable_class(v: &Value) -> bool {
    matches!(v, Value::ArcBulk(_) | Value::Hash(_) | Value::SmallHashInline(_))
}

impl Store {
    /// The demotion twin of [`Store::try_evict_after_write`], called
    /// beside it from the write-commit sites. No-op unless tiering is
    /// on AND `used_memory` is past the watermark; then spills at most
    /// one batch (B3: a single write never funds an unbounded spill
    /// storm — continuation rides [`Store::demote_step`] on the tick).
    /// Returns keys demoted.
    #[inline]
    pub fn try_demote_after_write(&mut self) -> usize {
        match &self.tier {
            None => 0,
            Some(t) if self.used_memory <= watermark(t.budget) => 0,
            Some(_) => self.demote_batch(),
        }
    }

    /// Tick continuation of [`Store::try_demote_after_write`]: one more
    /// budgeted batch per shard tick while over the watermark.
    #[inline]
    pub fn demote_step(&mut self) -> usize {
        self.try_demote_after_write()
    }

    /// Bulk-load drain (T4 / B11): demote batch after batch until the
    /// store is back under the watermark or candidates run dry. Replay /
    /// snapshot-load / reshard call this every K applied frames — those
    /// paths are single-threaded, so draining more than one write-path
    /// batch per check is safe (there is no reactor to stall). Returns
    /// total keys demoted.
    pub fn demote_to_watermark(&mut self) -> usize {
        let mut total = 0usize;
        loop {
            let n = self.try_demote_after_write();
            if n == 0 {
                return total;
            }
            total += n;
        }
    }

    /// One budgeted demotion batch: sample → demote, ≤ [`SPILL_BATCH`]
    /// records, stop at the watermark or when sampling runs dry. Ends
    /// with the compaction trigger.
    fn demote_batch(&mut self) -> usize {
        let (target, policy) = {
            let t = self.tier.as_ref().expect("gated by caller");
            (watermark(t.budget), t.policy)
        };
        let mut demoted = 0usize;
        let mut misses = 0u32;
        while self.used_memory > target && demoted < SPILL_BATCH {
            let victim = crate::evict::sample_pick_with(self, policy, |e| {
                spillable_class(&e.value) && e.weight() >= MIN_SPILL_BYTES
            });
            match victim {
                None => break,
                Some(k) if self.demote_in_place(&k) => {
                    demoted += 1;
                    misses = 0;
                }
                Some(_) => {
                    misses += 1;
                    if misses >= 3 {
                        break;
                    }
                }
            }
        }
        if demoted > 0 {
            self.tier_compact();
        }
        demoted
    }

    /// Swap `key`'s live value for a [`ColdRef`] stub, appending the
    /// codec bytes to the vlog. Re-stamps `Entry::weight` to the stub's
    /// actual footprint, applies the `used_memory` delta, preserves
    /// `lru_clock`/TTL, touches nothing else — zero events, zero WATCH
    /// bumps, no hfttl clear (field TTLs stay RAM-resident while cold).
    /// `false` when the key is absent/expired/non-spillable or the
    /// append failed (value stays hot).
    pub(crate) fn demote_in_place(&mut self, key: &[u8]) -> bool {
        let Some((payload, tag)) = ({
            match self.map.get(key) {
                Some(e) if !e.is_expired(self.cached_clock, self.cached_ns) => {
                    tier_codec::encode(&e.value)
                }
                _ => None,
            }
        }) else {
            return false;
        };
        let Some(t) = self.tier.as_mut() else { return false };
        let Ok(vref) = t.vlog.append(key, &payload) else {
            // Disk refused the spill — keep the value hot; the caller's
            // loop counts this as a miss. Never a silent value drop.
            return false;
        };
        let key_heap = key_heap_bytes_for(key);
        let e = self.map.get_mut(key).expect("probed above");
        let old_w = e.weight();
        let value_w = old_w.saturating_sub(key_heap);
        let stub = ColdRef {
            offset: vref.offset,
            file_id: vref.file_id,
            len: vref.len,
            weight: value_w.min(u64::from(u32::MAX)) as u32,
            type_tag: tag,
            touched: 0,
        };
        let old_value = core::mem::replace(&mut e.value, Value::Cold(stub));
        e.set_weight(key_heap);
        crate::apply_delta(&mut self.used_memory, -(value_w as i64));
        let t = self.tier.as_mut().expect("still enabled");
        t.demotions_total += 1;
        t.cold_keys += 1;
        t.cold_bytes += u64::from(stub.weight);
        self.maybe_offload_drop(old_value);
        true
    }

    /// Materialize `key`'s cold value back into the map: pread +
    /// decode, swap the stub out, re-stamp weight from the decoded
    /// value, credit the record dead. Same nothing-else contract as
    /// [`Store::demote_in_place`]. `false` when the value is not Cold.
    pub(crate) fn promote_in_place(&mut self, key: &[u8]) -> bool {
        let cref = match self.map.get(key).map(|e| &e.value) {
            Some(Value::Cold(c)) => *c,
            _ => return false,
        };
        let value = self.tier_read_record(cref);
        let key_heap = key_heap_bytes_for(key);
        let new_w = key_heap + value.weight();
        let e = self.map.get_mut(key).expect("probed above");
        e.value = value;
        let delta = new_w as i64 - e.weight() as i64;
        e.set_weight(new_w);
        crate::apply_delta(&mut self.used_memory, delta);
        let t = self.tier.as_mut().expect("cold value ⇒ tiering on");
        t.vlog.note_dead(cref.vref());
        t.renames.remove(&(cref.file_id, cref.offset));
        t.promotions_total += 1;
        t.cold_keys = t.cold_keys.saturating_sub(1);
        t.cold_bytes = t.cold_bytes.saturating_sub(u64::from(cref.weight));
        true
    }

    /// The deterministic demotion seam for the B9 transparency suite
    /// (`KEVY_TEST_FORCE_DEMOTE` genre): demote `key` NOW, ignoring the
    /// watermark and the min-spill threshold (so inline hashes force
    /// too) — the suite drives cold state per-key, never by eviction
    /// timing. Returns whether a demotion happened.
    #[doc(hidden)]
    pub fn debug_force_demote(&mut self, key: &[u8]) -> bool {
        if self.tier.is_none() {
            return false;
        }
        self.demote_in_place(key)
    }

    /// Test-only direct compaction trigger (the rename-survival test
    /// needs a deterministic pass, not a batch side effect).
    #[cfg(test)]
    pub(crate) fn tier_force_compact_for_tests(&mut self) {
        self.tier_compact();
    }

    /// Post-batch compaction trigger: retire every sealed vlog file
    /// whose live ratio fell under [`COMPACT_LIVE_PCT`]. The owner
    /// resolves liveness through the map (a Cold entry with the exact
    /// ref), falling back through the rename forward-pointers.
    fn tier_compact(&mut self) {
        let Some(t) = self.tier.as_mut() else { return };
        let mut owner = StoreOwner { map: &mut self.map, renames: &mut t.renames };
        // An IO error mid-compaction leaves untouched files untouched;
        // surfaced loudly (per-boot spill file — a failure is a bug).
        t.vlog
            .compact_below(COMPACT_LIVE_PCT, &mut owner)
            .expect("tier: vlog compaction failed — per-boot spill file, this is a process bug");
    }
}

#[inline]
fn watermark(budget: u64) -> u64 {
    budget.saturating_mul(WATERMARK_NUM) / WATERMARK_DEN
}

/// [`CompactOwner`] over the store map + the rename forward-pointers.
struct StoreOwner<'a> {
    map: &'a mut kevy_map::KevyMap<SmallBytes, Entry>,
    renames: &'a mut std::collections::HashMap<(u32, u64), SmallBytes>,
}

impl StoreOwner<'_> {
    /// The key currently holding the stub for `old` — the record's own
    /// key, or the rename forward-pointer's target. `None` = dead.
    fn resolve(&self, key: &[u8], old: VlogRef) -> Option<SmallBytes> {
        if stub_matches(self.map.get(key), old) {
            return Some(SmallBytes::from_slice(key));
        }
        let fwd = self.renames.get(&(old.file_id, old.offset))?;
        stub_matches(self.map.get(fwd.as_slice()), old).then(|| fwd.clone())
    }
}

fn stub_matches(e: Option<&Entry>, old: VlogRef) -> bool {
    matches!(
        e.map(|e| &e.value),
        Some(Value::Cold(c)) if c.file_id == old.file_id && c.offset == old.offset
    )
}

impl CompactOwner for StoreOwner<'_> {
    fn is_live(&mut self, key: &[u8], old: VlogRef) -> bool {
        self.resolve(key, old).is_some()
    }

    fn moved(&mut self, key: &[u8], old: VlogRef, new: VlogRef) {
        let holder = self.resolve(key, old).expect("moved() only after is_live");
        let e = self.map.get_mut(holder.as_slice()).expect("resolved live");
        let Value::Cold(c) = &mut e.value else { unreachable!("resolved a stub") };
        c.file_id = new.file_id;
        c.offset = new.offset;
        c.len = new.len;
        self.renames.remove(&(old.file_id, old.offset));
        // The re-appended record still carries its original embedded
        // key; if the stub lives elsewhere, keep the forward pointer.
        if holder.as_slice() != key {
            self.renames.insert((new.file_id, new.offset), holder);
        }
    }
}
