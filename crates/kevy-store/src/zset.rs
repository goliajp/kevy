//! `Store` sorted-set commands.

use crate::util::*;
use crate::value::*;
use crate::{Entry, Store, StoreError};

impl Store {
    // ---- sorted sets ---------------------------------------------------

    fn zset_mut(&mut self, key: &[u8], create: bool) -> Result<Option<&mut ZSetData>, StoreError> {
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.map.insert(
                key.to_vec(),
                Entry {
                    value: Value::ZSet(Box::default()),
                    expire_at: None,
                },
            );
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::ZSet(z) => Ok(Some(z)),
            _ => Err(StoreError::WrongType),
        }
    }

    fn zset_ref(&mut self, key: &[u8]) -> Result<Option<&ZSetData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(Some(z)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    fn drop_if_empty_zset(&mut self, key: &[u8]) {
        if let Some(Value::ZSet(z)) = self.map.get(key).map(|e| &e.value)
            && z.len() == 0
        {
            self.map.remove(key);
        }
    }

    /// `ZADD` — returns the count of newly-added members (updates don't count).
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, Vec<u8>)]) -> Result<usize, StoreError> {
        let z = self.zset_mut(key, true)?.expect("created");
        Ok(pairs
            .iter()
            .filter(|(score, m)| z.insert(m, *score))
            .count())
    }

    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> Result<Option<f64>, StoreError> {
        Ok(self
            .zset_ref(key)?
            .and_then(|z| z.by_member.get(member).copied()))
    }

    pub fn zcard(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.zset_ref(key)?.map_or(0, |z| z.len()))
    }

    pub fn zrem(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let removed = match self.zset_mut(key, false)? {
            None => 0,
            Some(z) => members.iter().filter(|m| z.remove(m)).count(),
        };
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
        let z = self.zset_mut(key, true)?.expect("created");
        let next = z.by_member.get(member).copied().unwrap_or(0.0) + incr;
        z.insert(member, next);
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
