//! `Store` sorted-set range / pop / range-removal commands
//! (`ZRANGE` / `ZRANGEBYSCORE` / `ZCOUNT` / `ZPOPMIN` / `ZREMRANGEBY*`).
//! Split out of `zset.rs` to keep it under the 500-LOC house cap; the
//! write-path core (`ZADD` / `ZREM` / `ZINCRBY`) stays there.

use crate::util::range_bounds;
use crate::value::{ScoreBound, Value};
use crate::{Store, StoreError};

impl Store {
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

    /// `ZPOPMIN key [count]` — pop and return the `count` lowest-scored
    /// members (ascending by `(score, member)`). Returns `(member,
    /// score)` pairs in pop order; empty when the key is absent / empty.
    pub fn zpopmin(
        &mut self,
        key: &[u8],
        count: usize,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        if count == 0 {
            // Validate type up-front so ZPOPMIN k 0 against a wrong-type
            // key still reports WRONGTYPE (Redis behaviour).
            if let Some(e) = self.live_entry(key) {
                match &e.value {
                    Value::ZSet(_) | Value::SmallZSetInline(_) => {}
                    _ => return Err(StoreError::WrongType),
                }
            }
            return Ok(Vec::new());
        }
        // Snapshot the lowest `count` members first (immutable borrow),
        // then remove them via the shared zrem path (which handles the
        // encoding, weight accounting, and empty-key cleanup uniformly).
        let to_pop: Vec<(Vec<u8>, f64)> = match self.live_entry(key) {
            None => return Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::ZSet(z) => z
                    .ordered()
                    .take(count)
                    .map(|(m, sc)| (m.to_vec(), sc))
                    .collect(),
                Value::SmallZSetInline(z) => {
                    let mut entries: Vec<(Vec<u8>, f64)> =
                        z.iter().map(|(m, sc)| (m.to_vec(), sc)).collect();
                    entries.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                    entries.into_iter().take(count).collect()
                }
                _ => return Err(StoreError::WrongType),
            },
        };
        if to_pop.is_empty() {
            return Ok(to_pop);
        }
        let borrowed: Vec<&[u8]> = to_pop.iter().map(|(m, _)| m.as_slice()).collect();
        self.zrem(key, &borrowed)?;
        Ok(to_pop)
    }

    /// `zpopmin_below` — pop up to `count` lowest-scored members
    /// whose score is `< below` (strictly). The delayed-job primitive:
    /// "pop everything that is due" in one atomic call (score = due
    /// time, `below` = now). Absent key = empty; wrong type errors.
    pub fn zpopmin_below(
        &mut self,
        key: &[u8],
        below: f64,
        count: usize,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        if count == 0 {
            if let Some(e) = self.live_entry(key) {
                match &e.value {
                    Value::ZSet(_) | Value::SmallZSetInline(_) => {}
                    _ => return Err(StoreError::WrongType),
                }
            }
            return Ok(Vec::new());
        }
        let to_pop: Vec<(Vec<u8>, f64)> = match self.live_entry(key) {
            None => return Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::ZSet(z) => z
                    .ordered()
                    .take_while(|(_, sc)| *sc < below)
                    .take(count)
                    .map(|(m, sc)| (m.to_vec(), sc))
                    .collect(),
                Value::SmallZSetInline(z) => {
                    let mut entries: Vec<(Vec<u8>, f64)> =
                        z.iter().map(|(m, sc)| (m.to_vec(), sc)).collect();
                    entries.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                    entries
                        .into_iter()
                        .take_while(|(_, sc)| *sc < below)
                        .take(count)
                        .collect()
                }
                _ => return Err(StoreError::WrongType),
            },
        };
        if to_pop.is_empty() {
            return Ok(to_pop);
        }
        let borrowed: Vec<&[u8]> = to_pop.iter().map(|(m, _)| m.as_slice()).collect();
        self.zrem(key, &borrowed)?;
        Ok(to_pop)
    }

    /// `ZREMRANGEBYRANK key start stop` — remove members in the rank
    /// range `[start, stop]` (inclusive, negative indices count from
    /// the tail). Returns the number of members removed.
    pub fn zrem_range_by_rank(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<usize, StoreError> {
        let to_remove: Vec<Vec<u8>> = match self.live_entry(key) {
            None => return Ok(0),
            Some(e) => match &e.value {
                Value::ZSet(z) => match crate::util::range_bounds(start, stop, z.len()) {
                    None => return Ok(0),
                    Some((s, end)) => z
                        .ordered()
                        .skip(s)
                        .take(end - s + 1)
                        .map(|(m, _)| m.to_vec())
                        .collect(),
                },
                Value::SmallZSetInline(z) => {
                    let mut entries: Vec<(Vec<u8>, f64)> =
                        z.iter().map(|(m, sc)| (m.to_vec(), sc)).collect();
                    entries.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                    match crate::util::range_bounds(start, stop, entries.len()) {
                        None => return Ok(0),
                        Some((s, end)) => entries
                            .into_iter()
                            .skip(s)
                            .take(end - s + 1)
                            .map(|(m, _)| m)
                            .collect(),
                    }
                }
                _ => return Err(StoreError::WrongType),
            },
        };
        if to_remove.is_empty() {
            return Ok(0);
        }
        let borrowed: Vec<&[u8]> = to_remove.iter().map(Vec::as_slice).collect();
        self.zrem(key, &borrowed)
    }

    /// `ZREMRANGEBYSCORE key min max` — remove every member whose score
    /// satisfies `min ≤ score ≤ max` (with `(` for exclusive bounds via
    /// `ScoreBound`). Returns the number removed.
    pub fn zrem_range_by_score(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> Result<usize, StoreError> {
        // Reuse zrange_by_score's bound logic to materialise the hit set
        // — keeps inline / heap parity in one place.
        let hits = self.zrange_by_score(key, min, max)?;
        if hits.is_empty() {
            // Still need to honour wrong-type errors that zrange_by_score
            // already surfaced; here Ok([]) means empty match, not type
            // mismatch, so it's safe to early-return.
            return Ok(0);
        }
        let borrowed: Vec<&[u8]> = hits.iter().map(|(m, _)| m.as_slice()).collect();
        self.zrem(key, &borrowed)
    }

    /// `ZREVRANGEBYSCORE` — `zrange_by_score` reversed. Bounds are
    /// passed in the `(min, max)` order already (the caller is
    /// responsible for swapping the user-facing `max first, min second`
    /// at the dispatch layer).
    pub fn zrev_range_by_score(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        let mut v = self.zrange_by_score(key, min, max)?;
        v.reverse();
        Ok(v)
    }
}
