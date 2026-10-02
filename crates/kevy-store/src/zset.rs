//! `Store` sorted-set write-path commands (`ZADD` / `ZREM` / `ZINCRBY` /
//! `ZSCORE` / `ZCARD` / `ZRANK`). The range / pop / range-removal family
//! lives in `zset_range.rs` (500-LOC house cap).

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::small_zset::{self, AddResult as ZAddResult, SmallZSetData};
use crate::value::{SmallBytes, Value, ZSetData};
use crate::zset_seg::{SegZSetData, Z_PROMOTE};
use crate::{Entry, Store, StoreError};
use alloc::sync::Arc;

/// `-0.0` and `0.0` are one score, as they are in Redis.
///
/// Folded at the one door every score passes through to reach a sorted set,
/// rather than inside `Score`'s comparison — that would be a branch in the
/// innermost step of every tree descent, paid on reads as well as writes, to
/// settle something that can be settled once on the way in.
///
/// Redis keys its skiplist on a plain `double` compare, where the two zeros
/// are equal and the member breaks the tie. This rank tree is a B-tree,
/// whose key needs a total order that `f64: PartialOrd` is not, and `Score`
/// orders by `total_cmp` — which is total precisely because it separates
/// them. Measured against a real `redis:8` on one host,
/// `ZADD z 0 a; ZADD z -0 b; ZRANGE z 0 -1` answered `a b` there and `b a`
/// here. See the finding under bench/.
#[inline]
fn fold_zero_sign(score: f64) -> f64 {
    if score == 0.0 { 0.0 } else { score }
}

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
        let Some(e) = self.map.get_mut_quiet(key) else { return };
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

    fn drop_if_empty_zset(&mut self, key: &[u8]) {
        let empty = match self.map.get(key).map(|e| &e.value) {
            Some(Value::ZSet(z)) => z.len() == 0,
            Some(Value::SegZSet(z)) => z.is_empty(),
            Some(Value::SmallZSetInline(z)) => z.is_empty(),
            _ => false,
        };
        if empty {
            self.remove_emptied(key);
        }
    }

    /// `ZADD` — returns the count of newly-added members. Borrowed
    /// argv: no per-member allocation; routes through the
    /// encoding-switch path.
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> Result<usize, StoreError> {
        let Some(((score0, m0), rest)) = pairs.split_first() else {
            return Ok(0);
        };
        // one probe for the whole command: every member then reaches the
        // zset through its slot, which no member's insert can move
        let (slot, mut added, todo) = match self.live_slot(key) {
            Some(slot) => (slot, 0usize, pairs),
            None => (self.zadd_create(key, m0, fold_zero_sign(*score0)), 1, rest),
        };
        let mut delta: i64 = 0;
        for (score, m) in todo {
            match self.zadd_at(key, slot, m, *score)? {
                ZaddOutcome::AddedInline => added += 1,
                ZaddOutcome::UpdatedInline => {}
                ZaddOutcome::AddedHeap(w) => {
                    added += 1;
                    delta += w;
                }
                ZaddOutcome::UpdatedHeap(w) => delta += w,
            }
        }
        self.account_delta_at(Some(slot), delta);
        Ok(added)
    }

    /// One member's score. `Ok(None)` for a missing key or a missing
    /// member — ZSCORE does not distinguish them.
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

    /// Member count. A missing key is 0, matching ZCARD.
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
    pub fn zrem(&mut self, key: &[u8], members: &[&[u8]]) -> Result<usize, StoreError> {
        let (removed, delta) = {
            let mut r = 0usize;
            let mut d: i64 = 0;
            if let Some(e) = self.live_entry_mut(key) {
                match &mut e.value {
                    // the member table does not shrink: a member takes its
                    // rank-tree slot and its heap
                    Value::ZSet(z) => {
                        // G-A3: hoist Arc::make_mut OUT of loop.
                        let z = Arc::make_mut(z);
                        for m in members {
                            if z.remove(m) {
                                r += 1;
                                d -= member_weight(m);
                            }
                        }
                    }
                    Value::SegZSet(z) => {
                        let z = Arc::make_mut(z);
                        for m in members {
                            if let Some(shell) = z.remove_weighed(m) {
                                r += 1;
                                d += shell - member_weight(m);
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
                Value::ZSet(z) => {
                    Ok(z.by_member.get(member).copied().and_then(|sc| z.rank_of(member, sc)))
                }
                Value::SegZSet(z) => Ok(z.score_of(member).and_then(|sc| z.rank_of(member, sc))),
                Value::SmallZSetInline(z) => {
                    // Inline holds at most 2 entries; sort by score (then
                    // bytes) so ZRANK matches ZRANGE order.
                    let mut entries: Vec<(&[u8], f64)> = z.iter().collect();
                    entries.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(b.0)));
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
        let (is_new, grown) = z.insert_weighed(member, next);
        let d = grown + if is_new { member_weight(member) } else { 0 };
        self.account_delta(key, d);
        Ok(next)
    }

    /// A.8 core: set one `(member, score)` pair via encoding-switch.
    fn zadd_at(
        &mut self,
        key: &[u8],
        at: usize,
        m: &[u8],
        score: f64,
    ) -> Result<ZaddOutcome, StoreError> {
        let score = fold_zero_sign(score);
        let v = &mut self.map.entry_at_mut(key, at).expect("the slot live_slot found").value;
        match v {
            Value::SmallZSetInline(z) => match z.try_set(m, score) {
                ZAddResult::Added => Ok(ZaddOutcome::AddedInline),
                ZAddResult::Updated => Ok(ZaddOutcome::UpdatedInline),
                ZAddResult::NoRoom => {
                    let outcome = promote_inline_zset_and_add(v, m, score);
                    self.reweigh_at(at, None);
                    Ok(outcome)
                }
            },
            Value::ZSet(z) if z.len() >= Z_PROMOTE => {
                let is_new = promote_flat_zset_and_add(v, m, score);
                self.reweigh_at(at, None);
                // Reweighed from scratch — swallow the per-member delta.
                if is_new { Ok(ZaddOutcome::AddedHeap(0)) } else { Ok(ZaddOutcome::UpdatedHeap(0)) }
            }
            Value::ZSet(z) => Ok(added(flat_insert_weighed(Arc::make_mut(z), m, score), m)),
            Value::SegZSet(z) => Ok(added(Arc::make_mut(z).insert_weighed(m, score), m)),
            _ => Err(StoreError::WrongType),
        }
    }

    /// Create a fresh entry holding one `(member, score)` pair; its slot.
    fn zadd_create(&mut self, key: &[u8], m: &[u8], score: f64) -> usize {
        let value = match SmallZSetData::with_one(m, score) {
            Some(inline) => Value::SmallZSetInline(inline),
            None => {
                let mut z = ZSetData::default();
                z.insert(m, score);
                Value::ZSet(Arc::new(z))
            }
        };
        self.insert_entry_at(SmallBytes::from_slice(key), Entry::new(value, None)).0
    }
}

/// Inline zset out of room: promote to the flat heap encoding, then
/// set the spilling pair. Caller reweighs the entry.
fn promote_inline_zset_and_add(v: &mut Value, m: &[u8], score: f64) -> ZaddOutcome {
    let Value::SmallZSetInline(z) = v else { unreachable!("matched inline") };
    let mut promoted = small_zset::promote(z);
    let is_new = promoted.insert(m, score);
    *v = Value::ZSet(Arc::new(promoted));
    // the caller reweighs from scratch, which counts the member
    if is_new { ZaddOutcome::AddedHeap(0) } else { ZaddOutcome::UpdatedHeap(0) }
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
    /// Insert or update; whether the member was new, and by how many
    /// bytes the structure around the members moved.
    fn insert_weighed(&mut self, member: &[u8], score: f64) -> (bool, i64) {
        match self {
            Self::Flat(z) => flat_insert_weighed(z, member, score),
            Self::Seg(z) => z.insert_weighed(member, score),
        }
    }
}

/// [`ZSetData::insert`], also answering by how many bytes its member table
/// grew (the rank tree is charged per member, in [`member_weight`]).
fn flat_insert_weighed(z: &mut ZSetData, member: &[u8], score: f64) -> (bool, i64) {
    let before = z.by_member.footprint();
    let is_new = z.insert(member, score);
    (is_new, z.by_member.footprint() as i64 - before as i64)
}

/// What a member adds to a zset besides the structure's growth: its
/// rank-tree slot, and its heap held twice (member table and rank tree).
fn member_weight(m: &[u8]) -> i64 {
    let heap = kevy_map::malloc_footprint(SmallBytes::heap_bytes_for(m)) as u64;
    (2 * heap + crate::value::RANKTREE_SLOT_BYTES) as i64
}

/// A ZADD verdict from an insert into a heap encoding.
fn added((is_new, grown): (bool, i64), m: &[u8]) -> ZaddOutcome {
    if is_new {
        ZaddOutcome::AddedHeap(member_weight(m) + grown)
    } else {
        ZaddOutcome::UpdatedHeap(grown)
    }
}

enum ZaddOutcome {
    AddedInline,
    UpdatedInline,
    /// What the new member and any growth it caused added.
    AddedHeap(i64),
    /// What an update moved the structure by (a segment split or retired).
    UpdatedHeap(i64),
}
