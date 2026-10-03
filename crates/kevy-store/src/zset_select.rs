//! Range reads that walk only what they return. [`Store::zrange_select`]
//! turns a rank, score or byte span, a direction and a `LIMIT` into two
//! ascending ranks first — two descents, whatever the span — and then
//! iterates exactly the entries between them, borrowing them in place.

use kevy_ranktree::{Iter as TreeIter, IterRev as TreeIterRev};

use crate::util::range_bounds;
use crate::value::{Score, ScoreBound, SmallBytes};
use crate::zindex;
use crate::zset_lex::LexEnd;
use crate::{Store, StoreError, Value};

/// Which part of a sorted set a range read takes.
///
/// ```
/// use kevy_store::{ScoreBound, ZSpan};
/// let span = ZSpan::Score(ScoreBound::inclusive(1.0), ScoreBound::exclusive(2.0));
/// assert!(matches!(span, ZSpan::Score(..)));
/// ```
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum ZSpan<'a> {
    /// Ranks `start..=stop` as `ZRANGE` indexes them: negative from the
    /// end, counted from the top when the read is reversed.
    Rank(i64, i64),
    /// Scores at or past the first bound and at or before the second.
    Score(ScoreBound, ScoreBound),
    /// Members at or past the first bound and at or before the second,
    /// by bytes — for a set whose members share one score.
    Lex(LexEnd<'a>, LexEnd<'a>),
}

/// The entries a range read selected, in its direction; see
/// [`Store::zrange_select`].
#[derive(Debug)]
pub struct ZRange<'a> {
    walk: Walk<'a>,
    left: usize,
}

#[derive(Debug)]
enum Walk<'a> {
    Small { e: [(&'a [u8], f64); 2], lo: usize, hi: usize, rev: bool },
    Flat(TreeIter<'a, (Score, SmallBytes)>),
    FlatRev(TreeIterRev<'a, (Score, SmallBytes)>),
    Seg(zindex::Iter<'a>),
    SegRev(zindex::IterRev<'a>),
}

impl<'a> Iterator for ZRange<'a> {
    type Item = (&'a [u8], f64);

    fn next(&mut self) -> Option<Self::Item> {
        self.left = self.left.checked_sub(1)?;
        match &mut self.walk {
            Walk::Small { e, lo, hi, rev } => {
                let i = if *rev {
                    *hi -= 1;
                    *hi
                } else {
                    *lo += 1;
                    *lo - 1
                };
                e.get(i).copied()
            }
            Walk::Flat(it) => it.next().map(|(s, m)| (m.as_slice(), s.0)),
            Walk::FlatRev(it) => it.next().map(|(s, m)| (m.as_slice(), s.0)),
            Walk::Seg(it) => it.next(),
            Walk::SegRev(it) => it.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for ZRange<'_> {}

/// Ascending ranks `[lo, hi)` of `span` in a set of `len`, a `LIMIT`
/// (offset, count; a negative count is no cap) taken in the read's
/// direction. `bracket` answers a score or byte span as two ranks.
fn window(
    len: usize,
    span: ZSpan<'_>,
    rev: bool,
    limit: Option<(i64, i64)>,
    bracket: impl Fn(ZSpan<'_>) -> (usize, usize),
) -> (usize, usize) {
    let (mut lo, mut hi) = match span {
        ZSpan::Rank(a, b) => match range_bounds(a, b, len) {
            None => (0, 0),
            Some((s, e)) if rev => (len - 1 - e, len - s),
            Some((s, e)) => (s, e + 1),
        },
        other => bracket(other),
    };
    hi = hi.max(lo);
    if let Some((off, count)) = limit {
        let n = hi - lo;
        let off = usize::try_from(off).unwrap_or(n).min(n);
        let take = usize::try_from(count).map_or(n - off, |c| c.min(n - off));
        if rev {
            hi -= off;
            lo = hi - take;
        } else {
            lo += off;
            hi = lo + take;
        }
    }
    (lo, hi)
}

impl Store {
    /// The entries of `key` in `span`, ascending or (`rev`) descending,
    /// after `limit`'s offset and up to its count. A missing key selects
    /// nothing. The entries are borrowed: nothing is copied or allocated.
    ///
    /// ```
    /// use kevy_store::{ScoreBound, Store, ZSpan};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b"), (3.0, b"c")])?;
    /// let top: Vec<_> = s.zrange_select(b"z", ZSpan::Rank(0, 1), true, None)?.collect();
    /// assert_eq!(top, [(&b"c"[..], 3.0), (b"b", 2.0)]);
    /// let span = ZSpan::Score(ScoreBound::exclusive(1.0), ScoreBound::inclusive(f64::INFINITY));
    /// let r = s.zrange_select(b"z", span, false, Some((1, 5)))?;
    /// assert_eq!(r.len(), 1, "sized before walking");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zrange_select(
        &mut self,
        key: &[u8],
        span: ZSpan<'_>,
        rev: bool,
        limit: Option<(i64, i64)>,
    ) -> Result<ZRange<'_>, StoreError> {
        match self.live_entry(key) {
            Some(e) => select_in(&e.value, span, rev, limit),
            None => {
                Ok(ZRange { walk: Walk::Small { e: [(&[], 0.0); 2], lo: 0, hi: 0, rev }, left: 0 })
            }
        }
    }

    /// The entries of `key` in each of `spans`, ascending, to `f` —
    /// borrowed for as long as the store is, so a caller can keep them
    /// past one span while it reads the next. A missing key selects
    /// nothing.
    ///
    /// ```
    /// use kevy_store::{ScoreBound, Store, ZSpan};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice()), (5.0, b"b"), (9.0, b"c")])?;
    /// let low = ZSpan::Score(ScoreBound::inclusive(0.0), ScoreBound::inclusive(2.0));
    /// let high = ZSpan::Score(ScoreBound::inclusive(8.0), ScoreBound::inclusive(10.0));
    /// let mut got: Vec<&[u8]> = Vec::new();
    /// s.zrange_each_span(b"z", &[low, high], |m, _| got.push(m))?;
    /// assert_eq!(got, [&b"a"[..], b"c"]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zrange_each_span<'s>(
        &'s mut self,
        key: &[u8],
        spans: &[ZSpan<'_>],
        mut f: impl FnMut(&'s [u8], f64),
    ) -> Result<(), StoreError> {
        let Some(e) = self.live_entry(key) else { return Ok(()) };
        for span in spans {
            select_in(&e.value, *span, false, None)?.for_each(|(m, s)| f(m, s));
        }
        Ok(())
    }
}

impl Store {
    /// Remove the members of `key` in `span`, ascending, none of them
    /// copied out: the window is bracketed as two ranks, then the member
    /// at its first rank goes, as many times as the window is wide — a
    /// short member held inline while it goes. How many went.
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
        let (lo, hi) = match self.live_entry(key) {
            None => return Ok(0),
            Some(e) => bounds_in(&e.value, span)?,
        };
        for _ in lo..hi {
            let gone = {
                let mut at =
                    self.zrange_select(key, ZSpan::Rank(lo as i64, lo as i64), false, None)?;
                let Some((m, _)) = at.next() else { break };
                SmallBytes::from_slice(m)
            };
            self.zrem(key, &[gone.as_slice()])?;
        }
        Ok(hi - lo)
    }
}

/// The ascending ranks `[lo, hi)` of `span` in a sorted set value.
fn bounds_in(value: &Value, span: ZSpan<'_>) -> Result<(usize, usize), StoreError> {
    Ok(match value {
        Value::ZSet(z) => window(z.len(), span, false, None, |s| flat_bracket(z, s)),
        Value::SegZSet(z) => window(z.len(), span, false, None, |s| seg_bracket(z, s)),
        Value::SmallZSetInline(z) => {
            let mut e = [(&[][..], 0.0); 2];
            let n = z.iter().zip(e.iter_mut()).map(|(x, slot)| *slot = x).count();
            e[..n].sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(b.0)));
            window(n, span, false, None, |s| small_bracket(&e[..n], s))
        }
        _ => return Err(StoreError::WrongType),
    })
}

/// [`Store::zrange_select`] over a value already found live.
fn select_in<'s>(
    value: &'s Value,
    span: ZSpan<'_>,
    rev: bool,
    limit: Option<(i64, i64)>,
) -> Result<ZRange<'s>, StoreError> {
    Ok(match value {
        Value::ZSet(z) => {
            let (lo, hi) = window(z.len(), span, rev, limit, |s| flat_bracket(z, s));
            let walk = if rev {
                Walk::FlatRev(z.by_score.iter_rev_from(hi))
            } else {
                Walk::Flat(z.by_score.iter_from(lo))
            };
            ZRange { walk, left: hi - lo }
        }
        Value::SegZSet(z) => {
            let (lo, hi) = window(z.len(), span, rev, limit, |s| seg_bracket(z, s));
            let order = z.order();
            let walk = if rev {
                Walk::SegRev(order.iter_rev_through(hi))
            } else {
                Walk::Seg(order.iter_from(lo))
            };
            ZRange { walk, left: hi - lo }
        }
        Value::SmallZSetInline(z) => {
            let mut e = [(&[][..], 0.0); 2];
            let n = z.iter().zip(e.iter_mut()).map(|(x, slot)| *slot = x).count();
            e[..n].sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(b.0)));
            let (lo, hi) = window(n, span, rev, limit, |s| small_bracket(&e[..n], s));
            ZRange { walk: Walk::Small { e, lo, hi, rev }, left: hi - lo }
        }
        _ => return Err(StoreError::WrongType),
    })
}

fn flat_bracket(z: &crate::value::ZSetData, span: ZSpan<'_>) -> (usize, usize) {
    match span {
        ZSpan::Score(min, max) => (z.score_start_rank(&min), z.score_end_rank(&max)),
        ZSpan::Lex(min, max) => (
            z.by_score.partition_point(|(_, m)| !min.admits_from_below(m.as_slice())),
            z.by_score.partition_point(|(_, m)| max.admits_from_above(m.as_slice())),
        ),
        ZSpan::Rank(..) => (0, 0),
    }
}

fn seg_bracket(z: &crate::zset_seg::SegZSetData, span: ZSpan<'_>) -> (usize, usize) {
    match span {
        ZSpan::Score(min, max) => (z.score_start_rank(&min), z.score_end_rank(&max)),
        ZSpan::Lex(min, max) => (
            z.order().partition_keys(|_, m| !min.admits_from_below(m)),
            z.order().partition_keys(|_, m| max.admits_from_above(m)),
        ),
        ZSpan::Rank(..) => (0, 0),
    }
}

fn small_bracket(e: &[(&[u8], f64)], span: ZSpan<'_>) -> (usize, usize) {
    match span {
        ZSpan::Score(min, max) => {
            (e.partition_point(|x| !min.ge_ok(x.1)), e.partition_point(|x| max.le_ok(x.1)))
        }
        ZSpan::Lex(min, max) => (
            e.partition_point(|x| !min.admits_from_below(x.0)),
            e.partition_point(|x| max.admits_from_above(x.0)),
        ),
        ZSpan::Rank(..) => (0, 0),
    }
}
