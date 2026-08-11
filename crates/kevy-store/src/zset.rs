//! `Store` sorted-set write-path commands (`ZADD` / `ZREM` / `ZINCRBY` /
//! `ZSCORE` / `ZCARD` / `ZRANK`). The range / pop / range-removal family
//! lives in `zset_range.rs` (500-LOC house cap).

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::small_zset::{self, AddResult as ZAddResult, SmallZSetData};
use crate::zset_seg::{SegZSetData, Z_PROMOTE};
use crate::value::{ZSetData, SmallBytes, Value, zset_member_weight};
use crate::{Entry, Store, StoreError};
use alloc::sync::Arc;

impl Store {
    // ---- sorted sets ---------------------------------------------------

    /// Borrow the key's zset mutably; promotes inline → flat, and flat →
    /// segmented at the threshold (so ZINCRBY-only workloads cross the
    /// boundary too).
    fn zset_mut(&mut self, key: &[u8], create: bool) -> Result<Option<ZRefMut<'_>>, StoreError> {
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::ZSet(Arc::default()), None),
            );
        }
        // A.8: see hash.rs::hash_mut — promote out-of-scope.
        let needs = match self.map.get(key).map(|e| &e.value) {
            Some(Value::SmallZSetInline(_)) => true,
            Some(Value::ZSet(z)) => z.len() >= Z_PROMOTE,
            _ => false,
        };
        if needs {
            self.promote_zset_encoding(key);
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::ZSet(z) => Ok(Some(ZRefMut::Flat(Arc::make_mut(z)))),
            Value::SegZSet(z) => Ok(Some(ZRefMut::Seg(Arc::make_mut(z)))),
            _ => Err(StoreError::WrongType),
        }
    }

    /// One promotion step: inline → flat, or flat-at-threshold →
    /// segmented. Reweighs the entry.
    fn promote_zset_encoding(&mut self, key: &[u8]) {
        let Some(e) = self.map.get_mut(key) else { return };
        match &mut e.value {
            Value::SmallZSetInline(s) => {
                e.value = Value::ZSet(Arc::new(small_zset::promote(s)));
            }
            Value::ZSet(z) => {
                e.value = Value::SegZSet(Arc::new(SegZSetData::from_flat(z)));
            }
            _ => return,
        }
        self.reweigh_entry(key);
    }

    /// A.8: read the key's zset slot for ZADD. None when absent.
    fn zset_value_for_set(&mut self, key: &[u8]) -> Result<Option<&mut Value>, StoreError> {
        match self.live_entry_mut(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::ZSet(_) | Value::SegZSet(_) | Value::SmallZSetInline(_) => {
                    Ok(Some(&mut e.value))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    fn drop_if_empty_zset(&mut self, key: &[u8]) {
        let empty = match self.map.get(key).map(|e| &e.value) {
            Some(Value::ZSet(z)) => z.len() == 0,
            Some(Value::SegZSet(z)) => z.is_empty(),
            Some(Value::SmallZSetInline(z)) => z.is_empty(),
            _ => false,
        };
        if empty {
            self.remove_entry(key);
        }
    }

    /// `ZADD` — returns the count of newly-added members. Borrowed
    /// argv: no per-member allocation; routes through the
    /// encoding-switch path.
    pub fn zadd(
        &mut self,
        key: &[u8],
        pairs: &[(f64, &[u8])],
    ) -> Result<usize, StoreError> {
        if pairs.is_empty() {
            return Ok(0);
        }
        let mut added = 0usize;
        let mut delta: i64 = 0;
        for (score, m) in pairs {
            match self.zadd_one(key, m, *score)? {
                ZaddOutcome::AddedInline => added += 1,
                ZaddOutcome::UpdatedInline => {}
                ZaddOutcome::AddedHeap(w) => {
                    added += 1;
                    delta += w;
                }
                ZaddOutcome::UpdatedHeap => {}
            }
        }
        self.account_delta(key, delta);
        Ok(added)
    }

    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> Result<Option<f64>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(z.by_member.get(member).copied()),
                Value::SegZSet(z) => Ok(z.score_of(member)),
                Value::SmallZSetInline(z) => Ok(z.score(member)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn zcard(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        match self.live_entry(key) {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(z.len()),
                Value::SegZSet(z) => Ok(z.len()),
                Value::SmallZSetInline(z) => Ok(z.len()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `ZREM` — returns the count of members removed.
    pub fn zrem(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
    ) -> Result<usize, StoreError> {
        let (removed, delta) = {
            let mut r = 0usize;
            let mut d: i64 = 0;
            if let Some(e) = self.live_entry_mut(key) {
                match &mut e.value {
                    Value::ZSet(z) => {
                        // G-A3: hoist Arc::make_mut OUT of loop.
                        let z = Arc::make_mut(z);
                        for m in members {
                            if z.remove(m) {
                                r += 1;
                                d -= zset_member_weight(&SmallBytes::from_slice(m)) as i64;
                            }
                        }
                    }
                    Value::SegZSet(z) => {
                        let z = Arc::make_mut(z);
                        for m in members {
                            if z.remove(m) {
                                r += 1;
                                d -= zset_member_weight(&SmallBytes::from_slice(m)) as i64;
                            }
                        }
                    }
                    Value::SmallZSetInline(z) => {
                        for m in members {
                            if z.try_remove(m) {
                                r += 1;
                            }
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
            (r, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_zset(key);
        Ok(removed)
    }

    /// `ZRANK` — 0-based position in ascending order. O(log N): a hash
    /// lookup for the score, then one order-statistic tree descent.
    pub fn zrank(&mut self, key: &[u8], member: &[u8]) -> Result<Option<usize>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(z
                    .by_member
                    .get(member)
                    .copied()
                    .and_then(|sc| z.rank_of(member, sc))),
                Value::SegZSet(z) => {
                    Ok(z.score_of(member).and_then(|sc| z.rank_of(member, sc)))
                }
                Value::SmallZSetInline(z) => {
                    // Inline holds at most 2 entries; sort by score (then
                    // bytes) so ZRANK matches ZRANGE order.
                    let mut entries: Vec<(&[u8], f64)> = z.iter().collect();
                    entries.sort_by(|a, b| {
                        a.1.total_cmp(&b.1).then_with(|| a.0.cmp(b.0))
                    });
                    Ok(entries.iter().position(|(m, _)| *m == member))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `ZINCRBY` — add `incr` to a member's score; returns the new score.
    pub fn zincrby(&mut self, key: &[u8], incr: f64, member: &[u8]) -> Result<f64, StoreError> {
        let mut z = self.zset_mut(key, true)?.expect("created");
        let cur = z.score_of(member).unwrap_or(0.0);
        let next = cur + incr;
        let smb = SmallBytes::from_slice(member);
        let is_new = !z.contains_member(member);
        z.insert(member, next);
        let d = if is_new { zset_member_weight(&smb) as i64 } else { 0 };
        self.account_delta(key, d);
        Ok(next)
    }

    /// A.8 core: set one `(member, score)` pair via encoding-switch.
    fn zadd_one(&mut self, key: &[u8], m: &[u8], score: f64) -> Result<ZaddOutcome, StoreError> {
        if self.zset_value_for_set(key)?.is_none() {
            return Ok(self.zadd_create(key, m, score));
        }
        let v = self.zset_value_for_set(key)?.expect("present and a zset");
        match v {
            Value::SmallZSetInline(z) => match z.try_set(m, score) {
                ZAddResult::Added => Ok(ZaddOutcome::AddedInline),
                ZAddResult::Updated => Ok(ZaddOutcome::UpdatedInline),
                ZAddResult::NoRoom => {
                    let outcome = promote_inline_zset_and_add(v, m, score);
                    self.reweigh_entry(key);
                    Ok(outcome)
                }
            },
            Value::ZSet(z) if z.len() >= Z_PROMOTE => {
                let is_new = promote_flat_zset_and_add(v, m, score);
                self.reweigh_entry(key);
                // Reweighed from scratch — swallow the per-member delta.
                if is_new {
                    Ok(ZaddOutcome::AddedHeap(0))
                } else {
                    Ok(ZaddOutcome::UpdatedHeap)
                }
            }
            Value::ZSet(z) => {
                let z = Arc::make_mut(z);
                let smb = SmallBytes::from_slice(m);
                let w = zset_member_weight(&smb) as i64;
                if z.insert(m, score) {
                    Ok(ZaddOutcome::AddedHeap(w))
                } else {
                    Ok(ZaddOutcome::UpdatedHeap)
                }
            }
            Value::SegZSet(z) => {
                let z = Arc::make_mut(z);
                let smb = SmallBytes::from_slice(m);
                let w = zset_member_weight(&smb) as i64;
                if z.insert(m, score) {
                    Ok(ZaddOutcome::AddedHeap(w))
                } else {
                    Ok(ZaddOutcome::UpdatedHeap)
                }
            }
            _ => Err(StoreError::WrongType),
        }
    }

    /// Create a fresh entry holding one `(member, score)` pair.
    fn zadd_create(&mut self, key: &[u8], m: &[u8], score: f64) -> ZaddOutcome {
        if let Some(inline) = SmallZSetData::with_one(m, score) {
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::SmallZSetInline(inline), None),
            );
            ZaddOutcome::AddedInline
        } else {
            let mut z = ZSetData::default();
            z.insert(m, score);
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::ZSet(Arc::new(z)), None),
            );
            ZaddOutcome::AddedInline
        }
    }
}

/// Inline zset out of room: promote to the flat heap encoding, then
/// set the spilling pair. Caller reweighs the entry.
fn promote_inline_zset_and_add(v: &mut Value, m: &[u8], score: f64) -> ZaddOutcome {
    let Value::SmallZSetInline(z) = v else { unreachable!("matched inline") };
    let mut promoted = small_zset::promote(z);
    let smb = SmallBytes::from_slice(m);
    let is_new = !promoted.by_member.contains_key(m);
    let w = zset_member_weight(&smb) as i64;
    promoted.insert(m, score);
    *v = Value::ZSet(Arc::new(promoted));
    if is_new {
        ZaddOutcome::AddedHeap(w)
    } else {
        ZaddOutcome::UpdatedHeap
    }
}

/// Flat zset at the threshold: segment, then set. One-time
/// O(Z_PROMOTE) rebuild (or clone, if a view pins it now). Returns
/// whether the member was new; caller reweighs.
fn promote_flat_zset_and_add(v: &mut Value, m: &[u8], score: f64) -> bool {
    let Value::ZSet(z) = v else { unreachable!("matched ZSet") };
    let mut seg = SegZSetData::from_flat(z);
    let is_new = seg.insert(m, score);
    *v = Value::SegZSet(Arc::new(seg));
    is_new
}

/// A mutable borrow of either heap zset encoding — the read-modify-
/// write entry point (`zincrby`) stays encoding-blind.
enum ZRefMut<'a> {
    Flat(&'a mut ZSetData),
    Seg(&'a mut SegZSetData),
}

impl ZRefMut<'_> {
    fn score_of(&self, member: &[u8]) -> Option<f64> {
        match self {
            Self::Flat(z) => z.by_member.get(member).copied(),
            Self::Seg(z) => z.score_of(member),
        }
    }
    fn contains_member(&self, member: &[u8]) -> bool {
        match self {
            Self::Flat(z) => z.by_member.contains_key(member),
            Self::Seg(z) => z.contains_member(member),
        }
    }
    fn insert(&mut self, member: &[u8], score: f64) -> bool {
        match self {
            Self::Flat(z) => z.insert(member, score),
            Self::Seg(z) => z.insert(member, score),
        }
    }
}

enum ZaddOutcome {
    AddedInline,
    UpdatedInline,
    AddedHeap(i64),
    UpdatedHeap,
}
