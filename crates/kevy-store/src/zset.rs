//! `Store` sorted-set commands.

use crate::util::range_bounds;
use crate::value::{ZSetData, SmallBytes, Value, zset_member_weight, ScoreBound};
use crate::{Entry, Store, StoreError};
use std::sync::Arc;

impl Store {
    // ---- sorted sets ---------------------------------------------------

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
        match &mut self.map.get_mut(key).expect("present").value {
            Value::ZSet(z) => Ok(Some(Arc::make_mut(z))),
            _ => Err(StoreError::WrongType),
        }
    }

    fn zset_ref(&mut self, key: &[u8]) -> Result<Option<&ZSetData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(Some(z.as_ref())),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    fn drop_if_empty_zset(&mut self, key: &[u8]) {
        let empty = matches!(self.map.get(key).map(|e| &e.value), Some(Value::ZSet(z)) if z.len() == 0);
        if empty {
            self.remove_entry(key);
        }
    }

    /// G4 (v1.25): borrowed-pair `ZADD` — kills the per-member `Vec<u8>`
    /// allocs the dispatch layer used to do before calling [`Self::zadd`].
    pub fn zadd_borrowed(
        &mut self,
        key: &[u8],
        pairs: &[(f64, &[u8])],
    ) -> Result<usize, StoreError> {
        let (added, delta) = {
            let z = self.zset_mut(key, true)?.expect("created");
            let mut a = 0usize;
            let mut d: i64 = 0;
            for (score, m) in pairs {
                let smb = SmallBytes::from_slice(m);
                let w = zset_member_weight(&smb) as i64;
                if z.insert(*m, *score) {
                    a += 1;
                    d += w;
                }
            }
            (a, d)
        };
        self.account_delta(key, delta);
        Ok(added)
    }

    /// `ZADD` — returns the count of newly-added members (updates don't count).
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, Vec<u8>)]) -> Result<usize, StoreError> {
        let (added, delta) = {
            let z = self.zset_mut(key, true)?.expect("created");
            let mut a = 0usize;
            let mut d: i64 = 0;
            for (score, m) in pairs {
                let smb = SmallBytes::from_slice(m);
                let w = zset_member_weight(&smb) as i64;
                if z.insert(m, *score) {
                    a += 1;
                    d += w;
                }
                // Updating an existing score reuses the same member entry —
                // no weight delta (f64 score is a fixed 8 B already counted).
            }
            (a, d)
        };
        self.account_delta(key, delta);
        Ok(added)
    }

    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> Result<Option<f64>, StoreError> {
        Ok(self
            .zset_ref(key)?
            .and_then(|z| z.by_member.get(member).copied()))
    }

    pub fn zcard(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.zset_ref(key)?.map_or(0, super::value::ZSetData::len))
    }

    pub fn zrem(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let (removed, delta) = {
            let mut r = 0usize;
            let mut d: i64 = 0;
            if let Some(z) = self.zset_mut(key, false)? {
                for m in members {
                    if z.remove(m.as_slice()) {
                        r += 1;
                        d -= zset_member_weight(&SmallBytes::from_slice(m)) as i64;
                    }
                }
            }
            (r, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_zset(key);
        Ok(removed)
    }

    /// G4 (v1.25): borrowed-slice `ZREM` — see [`Self::sadd_borrowed`].
    pub fn zrem_borrowed(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
    ) -> Result<usize, StoreError> {
        let (removed, delta) = {
            let mut r = 0usize;
            let mut d: i64 = 0;
            if let Some(z) = self.zset_mut(key, false)? {
                for m in members {
                    if z.remove(*m) {
                        r += 1;
                        d -= zset_member_weight(&SmallBytes::from_slice(m)) as i64;
                    }
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
        Ok(self
            .zset_ref(key)?
            .and_then(|z| z.ordered().position(|(m, _)| m == member)))
    }

    /// `ZINCRBY` — add `incr` to a member's score (default 0), returns the new score.
    pub fn zincrby(&mut self, key: &[u8], incr: f64, member: &[u8]) -> Result<f64, StoreError> {
        let (next, delta) = {
            let z = self.zset_mut(key, true)?.expect("created");
            let cur = z.by_member.get(member).copied().unwrap_or(0.0);
            let next = cur + incr;
            let smb = SmallBytes::from_slice(member);
            let is_new = !z.by_member.contains_key(member);
            z.insert(member, next);
            let d = if is_new { zset_member_weight(&smb) as i64 } else { 0 };
            (next, d)
        };
        self.account_delta(key, delta);
        Ok(next)
    }

    /// `ZRANGE key start stop` by rank.
    pub fn zrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        match self.zset_ref(key)? {
            None => Ok(Vec::new()),
            Some(z) => Ok(match range_bounds(start, stop, z.len()) {
                None => Vec::new(),
                Some((s, e)) => z
                    .ordered()
                    .skip(s)
                    .take(e - s + 1)
                    .map(|(m, sc)| (m.to_vec(), sc))
                    .collect(),
            }),
        }
    }

    /// `ZRANGEBYSCORE` — members with score in the (possibly exclusive) bounds.
    pub fn zrange_by_score(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        Ok(self.zset_ref(key)?.map_or(Vec::new(), |z| {
            z.ordered()
                .filter(|(_, sc)| min.ge_ok(*sc) && max.le_ok(*sc))
                .map(|(m, sc)| (m.to_vec(), sc))
                .collect()
        }))
    }

    /// `ZCOUNT` — number of members with score in the bounds.
    pub fn zcount(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> Result<usize, StoreError> {
        Ok(self.zset_ref(key)?.map_or(0, |z| {
            z.ordered()
                .filter(|(_, sc)| min.ge_ok(*sc) && max.le_ok(*sc))
                .count()
        }))
    }
}
