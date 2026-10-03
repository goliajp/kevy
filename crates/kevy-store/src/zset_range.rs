//! `Store` sorted-set range / pop / range-removal commands
//! (`ZRANGE` / `ZRANGEBYSCORE` / `ZCOUNT` / `ZPOPMIN` / `ZREMRANGEBY*`).
//! Split out of `zset.rs` to keep it under the 500-LOC house cap; the
//! write-path core (`ZADD` / `ZREM` / `ZINCRBY`) stays there.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::{ScoreBound, SmallBytes, Value};
use crate::zset::member_weight;
use crate::{Store, StoreError, ZSpan};

impl Store {
    /// `ZRANGE key start stop` by rank.
    pub fn zrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        Ok(owned(self.zrange_select(key, ZSpan::Rank(start, stop), false, None)?))
    }

    /// `ZRANGEBYSCORE`.
    pub fn zrange_by_score(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        Ok(owned(self.zrange_select(key, ZSpan::Score(min, max), false, None)?))
    }

    /// `ZCOUNT`: two rank descents, nothing walked.
    pub fn zcount(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> Result<usize, StoreError> {
        Ok(self.zrange_select(key, ZSpan::Score(min, max), false, None)?.len())
    }

    /// `ZPOPMIN key [count]` — pop and return the `count` lowest-scored
    /// members (ascending by `(score, member)`). Returns `(member,
    /// score)` pairs in pop order; empty when the key is absent / empty.
    pub fn zpopmin(&mut self, key: &[u8], count: usize) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        let mut low = Vec::new();
        self.zpop_each(key, count, false, |m, s| low.push((m.to_vec(), s)))?;
        Ok(low)
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
                    Value::ZSet(_) | Value::SegZSet(_) | Value::SmallZSetInline(_) => {}
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
                Value::SegZSet(z) => z
                    .ordered()
                    .take_while(|(_, sc)| *sc < below)
                    .take(count)
                    .map(|(m, sc)| (m.to_vec(), sc))
                    .collect(),
                Value::SmallZSetInline(z) => {
                    let mut entries: Vec<(Vec<u8>, f64)> =
                        z.iter().map(|(m, sc)| (m.to_vec(), sc)).collect();
                    entries.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                    entries.into_iter().take_while(|(_, sc)| *sc < below).take(count).collect()
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
        self.zrem_span(key, ZSpan::Rank(start, stop))
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
        self.zrem_span(key, ZSpan::Score(min, max))
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
        Ok(owned(self.zrange_select(key, ZSpan::Score(min, max), true, None)?))
    }

    /// `ZREVRANGE key start stop` — the rank window counted from the
    /// high end, which [`ZSpan::Rank`] reads the way `range_bounds`
    /// does: a negative start floors at zero, only the end is capped,
    /// and a start past the last index is an empty window.
    pub fn zrevrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        Ok(owned(self.zrange_select(key, ZSpan::Rank(start, stop), true, None)?))
    }
}

/// A selection copied out.
pub(crate) fn owned(r: crate::ZRange<'_>) -> Vec<(Vec<u8>, f64)> {
    r.map(|(m, s)| (m.to_vec(), s)).collect()
}

impl Store {
    /// Remove the members of `key` in `span`, none of them copied out: the
    /// key is found once, the window bracketed as two ranks, then the
    /// member at its first rank goes, as many times as the window is wide
    /// — a short member held inline while it goes. How many went.
    ///
    /// ```
    /// use kevy_store::{ScoreBound, Store, ZSpan};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b"), (3.0, b"c")])?;
    /// let low = ZSpan::Score(ScoreBound::inclusive(0.0), ScoreBound::inclusive(2.0));
    /// assert_eq!(s.zrem_span(b"z", low)?, 2);
    /// assert_eq!(s.zcard(b"z")?, 1);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zrem_span(&mut self, key: &[u8], span: ZSpan<'_>) -> Result<usize, StoreError> {
        let Some(e) = self.live_entry_mut(key) else { return Ok(0) };
        let (lo, hi) = crate::zset_select::bounds_in(&e.value, span)?;
        if lo == hi {
            return Ok(0);
        }
        let delta = remove_ranks(&mut e.value, lo, hi - lo);
        self.account_delta(key, delta);
        self.drop_if_empty_zset(key);
        Ok(hi - lo)
    }
}

/// Remove `n` members from rank `lo` of a sorted set value; the weight it
/// shed.
fn remove_ranks(value: &mut Value, lo: usize, n: usize) -> i64 {
    let mut d = 0;
    match value {
        Value::ZSet(z) => {
            let z = alloc::sync::Arc::make_mut(z);
            for _ in 0..n {
                let Some((_, m)) = z.by_score.select(lo).cloned() else { break };
                z.remove(m.as_slice());
                d -= member_weight(m.as_slice());
            }
        }
        Value::SegZSet(z) => {
            let z = alloc::sync::Arc::make_mut(z);
            for _ in 0..n {
                let Some((m, _)) = z.ordered_from(lo).next() else { break };
                let m = SmallBytes::from_slice(m);
                if let Some(shell) = z.remove_weighed(m.as_slice()) {
                    d += shell - member_weight(m.as_slice());
                }
            }
        }
        Value::SmallZSetInline(z) => {
            let mut two = [(SmallBytes::new(), 0.0), (SmallBytes::new(), 0.0)];
            let k = z
                .iter()
                .zip(two.iter_mut())
                .map(|((m, s), slot)| *slot = (SmallBytes::from_slice(m), s))
                .count();
            two[..k].sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            for (m, _) in &two[lo..(lo + n).min(k)] {
                z.try_remove(m.as_slice());
            }
        }
        _ => {}
    }
    d
}
