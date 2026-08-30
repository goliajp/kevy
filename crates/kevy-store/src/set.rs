//! `Store` set write commands. Reads live in `set_read.rs`.
//!
//! Three encodings, promoted in order of size: `SmallSetInline` (≤8
//! tiny members, in the Value body) → `Set(Arc<KevySet>)` (flat heap) →
//! `SegSet` (bucket-sharded COW past [`crate::seg_map::HS_PROMOTE`]
//! members — a write under a live snapshot view clones one bucket, not
//! the whole value).

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::seg_map::{HS_PROMOTE, SegMap};
use crate::small_set::{AddResult, SmallSetData, promote};
use crate::value::{SetData, SmallBytes, Value, set_member_weight};
use crate::{Entry, Store, StoreError};
use alloc::sync::Arc;

impl Store {
    // ---- sets ----------------------------------------------------------

    /// Borrow the value at `key` for mutation. Returns `None` if the key
    /// is absent (and the caller creates) or `WrongType` on a non-set.
    fn set_value_mut(&mut self, key: &[u8]) -> Result<Option<&mut Value>, StoreError> {
        match self.live_entry_mut(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Set(_) | Value::SegSet(_) | Value::SmallSetInline(_) => {
                    Ok(Some(&mut e.value))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    fn drop_if_empty_set(&mut self, key: &[u8]) {
        let empty = match self.map.get(key).map(|e| &e.value) {
            Some(Value::Set(s)) => s.is_empty(),
            Some(Value::SegSet(s)) => s.is_empty(),
            Some(Value::SmallSetInline(s)) => s.is_empty(),
            _ => false,
        };
        if empty {
            self.remove_entry(key);
        }
    }

    /// `SADD` — returns the count of newly-added members.
    pub fn sadd(&mut self, key: &[u8], members: &[&[u8]]) -> Result<usize, StoreError> {
        if members.is_empty() {
            return Ok(0);
        }
        let mut added = 0usize;
        let mut delta: i64 = 0;
        for m in members {
            match self.sadd_one(key, m)? {
                SaddOutcome::AddedInline => added += 1,
                SaddOutcome::AddedHeap(w) => {
                    added += 1;
                    delta += w;
                }
                SaddOutcome::AlreadyPresent => {}
            }
        }
        self.account_delta(key, delta);
        Ok(added)
    }

    /// Insert one member; encapsulates the encoding-switch decision.
    fn sadd_one(&mut self, key: &[u8], m: &[u8]) -> Result<SaddOutcome, StoreError> {
        if self.set_value_mut(key)?.is_none() {
            return Ok(self.sadd_create(key, m));
        }
        let v = self.set_value_mut(key)?.expect("present and a set type");
        match v {
            Value::SmallSetInline(s) => match s.try_add(m) {
                AddResult::Added => Ok(SaddOutcome::AddedInline),
                AddResult::AlreadyPresent => Ok(SaddOutcome::AlreadyPresent),
                AddResult::NoRoom => {
                    let outcome = promote_inline_set_and_add(v, m);
                    self.reweigh_entry(key);
                    Ok(outcome)
                }
            },
            // Flat set at the threshold: shard, then add. One-time
            // O(HS_PROMOTE) re-bucket (or clone, if a view pins it now).
            Value::Set(s) if s.len() >= HS_PROMOTE => {
                let added = promote_flat_set_to_seg(v, m);
                self.reweigh_entry(key);
                // Reweighed from scratch — swallow the per-member delta.
                if added { Ok(SaddOutcome::AddedHeap(0)) } else { Ok(SaddOutcome::AlreadyPresent) }
            }
            Value::Set(s) => {
                let smb = SmallBytes::from_slice(m);
                let w = set_member_weight(&smb) as i64;
                if Arc::make_mut(s).insert(smb) {
                    Ok(SaddOutcome::AddedHeap(w))
                } else {
                    Ok(SaddOutcome::AlreadyPresent)
                }
            }
            Value::SegSet(s) => {
                let smb = SmallBytes::from_slice(m);
                let w = set_member_weight(&smb) as i64;
                if Arc::make_mut(s).insert(smb, ()).is_none() {
                    Ok(SaddOutcome::AddedHeap(w))
                } else {
                    Ok(SaddOutcome::AlreadyPresent)
                }
            }
            _ => Err(StoreError::WrongType),
        }
    }

    /// Create a fresh entry for `key` holding one member.
    fn sadd_create(&mut self, key: &[u8], m: &[u8]) -> SaddOutcome {
        if let Some(inline) = SmallSetData::with_one(m) {
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::SmallSetInline(inline), None),
            );
        } else {
            let smb = SmallBytes::from_slice(m);
            let mut s = SetData::with_capacity(1);
            s.insert(smb);
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::Set(Arc::new(s)), None),
            );
        }
        SaddOutcome::AddedInline
    }

    /// `SREM` — returns the count removed (deleting an emptied key).
    pub fn srem(&mut self, key: &[u8], members: &[&[u8]]) -> Result<usize, StoreError> {
        let (removed, delta) = {
            let mut r = 0usize;
            let mut d: i64 = 0;
            if let Some(v) = self.set_value_mut(key)? {
                match v {
                    Value::SmallSetInline(s) => {
                        for m in members {
                            if s.try_remove(m) {
                                r += 1;
                            }
                        }
                    }
                    Value::Set(s) => {
                        let set_mut = Arc::make_mut(s);
                        for m in members {
                            if set_mut.remove(*m) {
                                r += 1;
                                d -= set_member_weight(&SmallBytes::from_slice(m)) as i64;
                            }
                        }
                    }
                    Value::SegSet(s) => {
                        let set_mut = Arc::make_mut(s);
                        for m in members {
                            if set_mut.remove(m).is_some() {
                                r += 1;
                                d -= set_member_weight(&SmallBytes::from_slice(m)) as i64;
                            }
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
            (r, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_set(key);
        Ok(removed)
    }

    /// `SPOP key count` — remove and return up to `count` arbitrary
    /// members. Each draw starts at a random slot and takes the first
    /// occupied one — O(1) expected, Redis's `dictGetRandomKey` shape
    /// (sharded sets weight the bucket pick by length first).
    pub fn spop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut draws: Vec<u64> = (0..count).map(|_| self.rng.next_u64()).collect();
        let (out, delta) = {
            let mut o: Vec<Vec<u8>> = Vec::new();
            let mut d: i64 = 0;
            if let Some(v) = self.set_value_mut(key)? {
                match v {
                    Value::SmallSetInline(s) => {
                        let mut all: Vec<Vec<u8>> = s.iter_slices().map(<[u8]>::to_vec).collect();
                        let k = shuffle_prefix(&mut all, count, &mut draws);
                        all.truncate(k);
                        for m in &all {
                            s.try_remove(m.as_slice());
                        }
                        o = all;
                    }
                    Value::Set(s) => {
                        (o, d) = flat_spop_draws(Arc::make_mut(s), &draws, count);
                    }
                    Value::SegSet(s) => {
                        (o, d) = seg_spop_draws(Arc::make_mut(s), &draws, count);
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
            (o, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_set(key);
        Ok(out)
    }
}

/// Inline set out of room: promote to KevySet, then insert the
/// spilling member. Caller reweighs the entry.
fn promote_inline_set_and_add(v: &mut Value, m: &[u8]) -> SaddOutcome {
    let Value::SmallSetInline(s) = v else { unreachable!("matched inline") };
    let mut promoted = promote(s);
    let smb = SmallBytes::from_slice(m);
    let w = set_member_weight(&smb) as i64;
    let inserted = promoted.insert(smb);
    debug_assert!(inserted, "promote re-inserts existing inline");
    *v = Value::Set(Arc::new(promoted));
    if inserted { SaddOutcome::AddedHeap(w) } else { SaddOutcome::AlreadyPresent }
}

/// Flat set at the promotion threshold: re-bucket, then add `m`.
/// Returns whether `m` was newly added. Caller reweighs the entry.
fn promote_flat_set_to_seg(v: &mut Value, m: &[u8]) -> bool {
    let Value::Set(s) = v else { unreachable!("matched Set") };
    let flat = Arc::try_unwrap(core::mem::take(s)).unwrap_or_else(|a| (*a).clone());
    let mut seg: SegMap<()> = SegMap::default();
    for member in flat.iter() {
        seg.insert(member.clone(), ());
    }
    let added = seg.insert(SmallBytes::from_slice(m), ()).is_none();
    *v = Value::SegSet(Arc::new(seg));
    added
}

/// The SPOP draw loop over a flat set. Returns `(popped, delta)`.
fn flat_spop_draws(set_mut: &mut SetData, draws: &[u64], count: usize) -> (Vec<Vec<u8>>, i64) {
    let (mut o, mut d) = (Vec::new(), 0i64);
    for slot in draws.iter().take(count) {
        if set_mut.is_empty() {
            break;
        }
        let Some(m) =
            set_mut.iter_from_slot(*slot as usize).next().map(kevy_bytes::SmallBytes::to_vec)
        else {
            break;
        };
        if set_mut.remove(m.as_slice()) {
            d -= set_member_weight(&SmallBytes::from_slice(&m)) as i64;
        }
        o.push(m);
    }
    (o, d)
}

/// The SPOP draw loop over a sharded set (weighted-bucket random).
fn seg_spop_draws(set_mut: &mut SegMap<()>, draws: &[u64], count: usize) -> (Vec<Vec<u8>>, i64) {
    let (mut o, mut d) = (Vec::new(), 0i64);
    for draw in draws.iter().take(count) {
        if set_mut.is_empty() {
            break;
        }
        let Some(m) = set_mut.rand_entry(*draw).map(|(m, ())| m.to_vec()) else {
            break;
        };
        if set_mut.remove(m.as_slice()).is_some() {
            d -= set_member_weight(&SmallBytes::from_slice(&m)) as i64;
        }
        o.push(m);
    }
    (o, d)
}

/// Per-member result for the inner [`Store::sadd_one`] step.
enum SaddOutcome {
    AddedInline,
    AddedHeap(i64),
    AlreadyPresent,
}

/// Fisher-Yates over the first `k` positions, using pre-drawn
/// randomness (drawn BEFORE the value borrow — `self.rng` is
/// unreachable inside).
pub(crate) fn shuffle_prefix<T>(items: &mut [T], k: usize, draws: &mut Vec<u64>) -> usize {
    let n = items.len();
    let k = k.min(n);
    for i in 0..k {
        let span = (n - i) as u64;
        let d = draws.pop().unwrap_or(i as u64);
        items.swap(i, i + crate::rng::below(d, span) as usize);
    }
    k
}
