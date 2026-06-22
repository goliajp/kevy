//! `Store` sorted-set commands.

use crate::small_zset::{self, AddResult as ZAddResult, SmallZSetData};
use crate::util::range_bounds;
use crate::value::{ZSetData, SmallBytes, Value, zset_member_weight, ScoreBound};
use crate::{Entry, Store, StoreError};
use std::sync::Arc;

impl Store {
    // ---- sorted sets ---------------------------------------------------

    /// Borrow the key's zset mutably; promote inline → heap if needed.
    fn zset_mut(&mut self, key: &[u8], create: bool) -> Result<Option<&mut ZSetData>, StoreError> {
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
        let is_inline = matches!(
            self.map.get(key).map(|e| &e.value),
            Some(Value::SmallZSetInline(_))
        );
        if is_inline {
            let promoted = {
                let e = self.map.get(key).expect("present");
                if let Value::SmallZSetInline(s) = &e.value {
                    small_zset::promote(s)
                } else {
                    unreachable!()
                }
            };
            self.map.get_mut(key).expect("present").value = Value::ZSet(Arc::new(promoted));
            self.reweigh_entry(key);
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::ZSet(z) => Ok(Some(Arc::make_mut(z))),
            _ => Err(StoreError::WrongType),
        }
    }

    /// A.8: read the key's zset slot for ZADD. None when absent.
    fn zset_value_for_set(&mut self, key: &[u8]) -> Result<Option<&mut Value>, StoreError> {
        match self.live_entry_mut(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::ZSet(_) | Value::SmallZSetInline(_) => Ok(Some(&mut e.value)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    fn drop_if_empty_zset(&mut self, key: &[u8]) {
        let empty = match self.map.get(key).map(|e| &e.value) {
            Some(Value::ZSet(z)) => z.len() == 0,
            Some(Value::SmallZSetInline(z)) => z.is_empty(),
            _ => false,
        };
        if empty {
            self.remove_entry(key);
        }
    }

    /// G4 (v1.25): borrowed-pair `ZADD`. A.8: encoding-switch.
    pub fn zadd_borrowed(
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
            match self.zadd_one(key, *m, *score)? {
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

    /// `ZADD` — returns the count of newly-added members.
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, Vec<u8>)]) -> Result<usize, StoreError> {
        let borrowed: Vec<(f64, &[u8])> =
            pairs.iter().map(|(s, m)| (*s, m.as_slice())).collect();
        self.zadd_borrowed(key, &borrowed)
    }

    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> Result<Option<f64>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(z.by_member.get(member).copied()),
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
                Value::SmallZSetInline(z) => Ok(z.len()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn zrem(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let borrowed: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
        self.zrem_borrowed(key, &borrowed)
    }

    /// G4 (v1.25): borrowed-slice `ZREM`. A.8: encoding-aware.
    pub fn zrem_borrowed(
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
                            if z.remove(*m) {
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

    /// `ZRANK` — 0-based position in ascending order (O(n) for now).
    pub fn zrank(&mut self, key: &[u8], member: &[u8]) -> Result<Option<usize>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(z.ordered().position(|(m, _)| m == member)),
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
        let z = self.zset_mut(key, true)?.expect("created");
        let cur = z.by_member.get(member).copied().unwrap_or(0.0);
        let next = cur + incr;
        let smb = SmallBytes::from_slice(member);
        let is_new = !z.by_member.contains_key(member);
        z.insert(member, next);
        let d = if is_new { zset_member_weight(&smb) as i64 } else { 0 };
        self.account_delta(key, d);
        Ok(next)
    }

    /// `ZRANGE key start stop` by rank.
    pub fn zrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(match range_bounds(start, stop, z.len()) {
                    None => Vec::new(),
                    Some((s, end)) => z
                        .ordered()
                        .skip(s)
                        .take(end - s + 1)
                        .map(|(m, sc)| (m.to_vec(), sc))
                        .collect(),
                }),
                Value::SmallZSetInline(z) => {
                    let mut entries: Vec<(Vec<u8>, f64)> =
                        z.iter().map(|(m, sc)| (m.to_vec(), sc)).collect();
                    entries.sort_by(|a, b| {
                        a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0))
                    });
                    Ok(match range_bounds(start, stop, entries.len()) {
                        None => Vec::new(),
                        Some((s, end)) => entries.into_iter().skip(s).take(end - s + 1).collect(),
                    })
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `ZRANGEBYSCORE`.
    pub fn zrange_by_score(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(z
                    .ordered()
                    .filter(|(_, sc)| min.ge_ok(*sc) && max.le_ok(*sc))
                    .map(|(m, sc)| (m.to_vec(), sc))
                    .collect()),
                Value::SmallZSetInline(z) => {
                    let mut entries: Vec<(Vec<u8>, f64)> = z
                        .iter()
                        .filter(|(_, sc)| min.ge_ok(*sc) && max.le_ok(*sc))
                        .map(|(m, sc)| (m.to_vec(), sc))
                        .collect();
                    entries.sort_by(|a, b| {
                        a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0))
                    });
                    Ok(entries)
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `ZCOUNT`.
    pub fn zcount(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> Result<usize, StoreError> {
        match self.live_entry(key) {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(z
                    .ordered()
                    .filter(|(_, sc)| min.ge_ok(*sc) && max.le_ok(*sc))
                    .count()),
                Value::SmallZSetInline(z) => Ok(z
                    .iter()
                    .filter(|(_, sc)| min.ge_ok(*sc) && max.le_ok(*sc))
                    .count()),
                _ => Err(StoreError::WrongType),
            },
        }
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
                    let mut promoted = small_zset::promote(z);
                    let smb = SmallBytes::from_slice(m);
                    let is_new = !promoted.by_member.contains_key(m);
                    let w = zset_member_weight(&smb) as i64;
                    promoted.insert(m, score);
                    *v = Value::ZSet(Arc::new(promoted));
                    self.reweigh_entry(key);
                    if is_new {
                        Ok(ZaddOutcome::AddedHeap(w))
                    } else {
                        Ok(ZaddOutcome::UpdatedHeap)
                    }
                }
            },
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

enum ZaddOutcome {
    AddedInline,
    UpdatedInline,
    AddedHeap(i64),
    UpdatedHeap,
}
