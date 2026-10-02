//! Sorted-set ranges by member (`ZRANGEBYLEX`, `ZLEXCOUNT`,
//! `ZREMRANGEBYLEX`, `ZRANGE … BYLEX`). Members order by bytes only
//! among equal scores, which is what a lexicographic range assumes: the
//! bounds are found by binary search over ranks, each probe one rank
//! descent. With mixed scores the answer is as unspecified as Redis says.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::Value;
use crate::{Store, StoreError};

/// One end of a lexicographic range: `-` / `+`, or `[member` / `(member`.
///
/// ```
/// use kevy_store::LexBound;
/// assert_eq!(LexBound::parse(b"[b"), Some(LexBound::Inclusive(b"b".to_vec())));
/// assert_eq!(LexBound::parse(b"-"), Some(LexBound::NegInf));
/// assert_eq!(LexBound::parse(b"b"), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LexBound {
    /// Below every member.
    NegInf,
    /// Above every member.
    PosInf,
    /// The member itself is in range.
    Inclusive(Vec<u8>),
    /// The member itself is not.
    Exclusive(Vec<u8>),
}

impl LexBound {
    /// The bound a range argument spells, or `None` when it is no bound.
    pub fn parse(b: &[u8]) -> Option<Self> {
        match b {
            b"-" => Some(Self::NegInf),
            b"+" => Some(Self::PosInf),
            [b'[', rest @ ..] => Some(Self::Inclusive(rest.to_vec())),
            [b'(', rest @ ..] => Some(Self::Exclusive(rest.to_vec())),
            _ => None,
        }
    }

    /// `m` is at or past this lower bound.
    fn admits_from_below(&self, m: &[u8]) -> bool {
        match self {
            Self::NegInf => true,
            Self::PosInf => false,
            Self::Inclusive(b) => m >= b.as_slice(),
            Self::Exclusive(b) => m > b.as_slice(),
        }
    }

    /// `m` is at or before this upper bound.
    fn admits_from_above(&self, m: &[u8]) -> bool {
        match self {
            Self::NegInf => false,
            Self::PosInf => true,
            Self::Inclusive(b) => m <= b.as_slice(),
            Self::Exclusive(b) => m < b.as_slice(),
        }
    }
}

/// The ranks `[lo, hi)` of a ranked sorted set's members within
/// `[min, max]` by bytes.
macro_rules! lex_span {
    ($z:expr, $min:expr, $max:expr) => {{
        let z = $z;
        let at = |r: usize, f: &dyn Fn(&[u8]) -> bool| {
            z.ordered_from(r).next().is_some_and(|(m, _)| f(m))
        };
        let lo = partition(z.len(), |r| at(r, &|m| !$min.admits_from_below(m)));
        let hi = partition(z.len(), |r| at(r, &|m| $max.admits_from_above(m)));
        (lo, hi)
    }};
}

/// The first rank in `0..len` at which `holds` stops being true, for a
/// `holds` that is true on a prefix.
fn partition(len: usize, holds: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if holds(mid) { lo = mid + 1 } else { hi = mid }
    }
    lo
}

impl Store {
    /// The members within `[min, max]` by bytes, in order, with scores.
    ///
    /// ```
    /// use kevy_store::LexBound;
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(0.0, b"a".as_slice()), (0.0, b"b"), (0.0, b"c")])?;
    /// let got = s.zrange_by_lex(b"z", &LexBound::Exclusive(b"a".to_vec()), &LexBound::PosInf)?;
    /// assert_eq!(got, [(b"b".to_vec(), 0.0), (b"c".to_vec(), 0.0)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zrange_by_lex(
        &mut self,
        key: &[u8],
        min: &LexBound,
        max: &LexBound,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        let Some(e) = self.live_entry(key) else { return Ok(Vec::new()) };
        macro_rules! ranked {
            ($z:expr) => {{
                let (lo, hi) = lex_span!($z, min, max);
                $z.ordered_from(lo)
                    .take(hi.saturating_sub(lo))
                    .map(|(m, s)| (m.to_vec(), s))
                    .collect()
            }};
        }
        Ok(match &e.value {
            Value::ZSet(z) => ranked!(z),
            Value::SegZSet(z) => ranked!(z),
            Value::SmallZSetInline(z) => {
                let mut v: Vec<(Vec<u8>, f64)> = z
                    .iter()
                    .filter(|(m, _)| min.admits_from_below(m) && max.admits_from_above(m))
                    .map(|(m, s)| (m.to_vec(), s))
                    .collect();
                v.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                v
            }
            _ => return Err(StoreError::WrongType),
        })
    }

    /// `ZLEXCOUNT` — how many members fall within `[min, max]` by bytes:
    /// two binary searches, nothing walked.
    ///
    /// ```
    /// use kevy_store::LexBound;
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(0.0, b"a".as_slice()), (0.0, b"b"), (0.0, b"c")])?;
    /// assert_eq!(s.zlexcount(b"z", &LexBound::Inclusive(b"b".to_vec()), &LexBound::PosInf)?, 2);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zlexcount(
        &mut self,
        key: &[u8],
        min: &LexBound,
        max: &LexBound,
    ) -> Result<usize, StoreError> {
        let Some(e) = self.live_entry(key) else { return Ok(0) };
        Ok(match &e.value {
            Value::ZSet(z) => {
                let (lo, hi) = lex_span!(z, min, max);
                hi.saturating_sub(lo)
            }
            Value::SegZSet(z) => {
                let (lo, hi) = lex_span!(z, min, max);
                hi.saturating_sub(lo)
            }
            Value::SmallZSetInline(z) => z
                .iter()
                .filter(|(m, _)| min.admits_from_below(m) && max.admits_from_above(m))
                .count(),
            _ => return Err(StoreError::WrongType),
        })
    }

    /// `ZREMRANGEBYLEX` — remove the members within `[min, max]` by bytes;
    /// how many went.
    pub fn zremrange_by_lex(
        &mut self,
        key: &[u8],
        min: &LexBound,
        max: &LexBound,
    ) -> Result<usize, StoreError> {
        let gone = self.zrange_by_lex(key, min, max)?;
        if gone.is_empty() {
            return Ok(0);
        }
        let members: Vec<&[u8]> = gone.iter().map(|(m, _)| m.as_slice()).collect();
        self.zrem(key, &members)
    }
}

#[cfg(test)]
mod tests {
    use super::LexBound;
    use crate::Store;

    fn bound(r: u64, lower: bool) -> LexBound {
        // the members' own alphabet and length, so a bound often IS a member
        let m = vec![b'a' + (r % 4) as u8, b'a' + ((r / 4) % 4) as u8];
        match r % 7 {
            0 if lower => LexBound::NegInf,
            0 => LexBound::PosInf,
            1..=3 => LexBound::Inclusive(m),
            _ => LexBound::Exclusive(m),
        }
    }

    // written apart from the code under test, so a shared mistake shows
    fn above(min: &LexBound, m: &[u8]) -> bool {
        match min {
            LexBound::NegInf => true,
            LexBound::PosInf => false,
            LexBound::Inclusive(b) => m.cmp(b) != std::cmp::Ordering::Less,
            LexBound::Exclusive(b) => m.cmp(b) == std::cmp::Ordering::Greater,
        }
    }

    fn below(max: &LexBound, m: &[u8]) -> bool {
        match max {
            LexBound::NegInf => false,
            LexBound::PosInf => true,
            LexBound::Inclusive(b) => m.cmp(b) != std::cmp::Ordering::Greater,
            LexBound::Exclusive(b) => m.cmp(b) == std::cmp::Ordering::Less,
        }
    }

    /// Every encoding answers as a filter over the members would, at sizes
    /// that land in each of them.
    #[test]
    fn lex_ranges_match_a_plain_filter_in_every_encoding() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for size in [3usize, 60, 700, 2000] {
            let mut s = Store::new();
            let members: Vec<Vec<u8>> = (0..size)
                .map(|_| {
                    let n = next();
                    let mut m = vec![b'a' + (n % 4) as u8, b'a' + ((n / 4) % 4) as u8];
                    // longer members on the large sets, so they span segments
                    m.extend((0..size / 200).map(|i| b'a' + ((n >> (8 + i)) % 26) as u8));
                    m
                })
                .collect();
            let pairs: Vec<(f64, &[u8])> = members.iter().map(|m| (0.0, m.as_slice())).collect();
            s.zadd(b"z", &pairs).unwrap();
            let mut all: Vec<Vec<u8>> = members.clone();
            all.sort();
            all.dedup();
            // every size still meets its encoding under miri, with fewer probes
            for _ in 0..if cfg!(miri) { 8 } else { 200 } {
                let (min, max) = (bound(next(), true), bound(next(), false));
                let want: Vec<Vec<u8>> =
                    all.iter().filter(|m| above(&min, m) && below(&max, m)).cloned().collect();
                let got: Vec<Vec<u8>> = s
                    .zrange_by_lex(b"z", &min, &max)
                    .unwrap()
                    .into_iter()
                    .map(|(m, _)| m)
                    .collect();
                assert_eq!(got, want, "size {size}, {min:?}..{max:?}");
                assert_eq!(s.zlexcount(b"z", &min, &max).unwrap(), want.len());
            }
        }
    }
}
