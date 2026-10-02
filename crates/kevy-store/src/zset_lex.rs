//! Sorted-set ranges by member (`ZRANGEBYLEX`, `ZLEXCOUNT`,
//! `ZREMRANGEBYLEX`, `ZRANGE … BYLEX`). Members order by bytes only
//! among equal scores, which is what a lexicographic range assumes: the
//! bounds are found by binary search over ranks, each probe one rank
//! descent. With mixed scores the answer is as unspecified as Redis says.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::{Store, StoreError, ZSpan};

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
        Some(match LexEnd::parse(b)? {
            LexEnd::NegInf => Self::NegInf,
            LexEnd::PosInf => Self::PosInf,
            LexEnd::Inclusive(m) => Self::Inclusive(m.to_vec()),
            LexEnd::Exclusive(m) => Self::Exclusive(m.to_vec()),
        })
    }

    /// The same bound, borrowed.
    ///
    /// ```
    /// use kevy_store::{LexBound, LexEnd};
    /// assert_eq!(LexBound::Exclusive(b"m".to_vec()).as_end(), LexEnd::Exclusive(b"m"));
    /// ```
    pub fn as_end(&self) -> LexEnd<'_> {
        match self {
            Self::NegInf => LexEnd::NegInf,
            Self::PosInf => LexEnd::PosInf,
            Self::Inclusive(m) => LexEnd::Inclusive(m),
            Self::Exclusive(m) => LexEnd::Exclusive(m),
        }
    }
}

/// One end of a range by bytes, borrowed from the argument that spells
/// it: what [`LexBound`] holds, without the copy.
///
/// ```
/// use kevy_store::LexEnd;
/// assert_eq!(LexEnd::parse(b"[b"), Some(LexEnd::Inclusive(b"b")));
/// assert_eq!(LexEnd::parse(b"+"), Some(LexEnd::PosInf));
/// assert_eq!(LexEnd::parse(b"b"), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LexEnd<'a> {
    /// Below every member.
    NegInf,
    /// Above every member.
    PosInf,
    /// The member itself is in range.
    Inclusive(&'a [u8]),
    /// The member itself is not.
    Exclusive(&'a [u8]),
}

impl<'a> LexEnd<'a> {
    /// The bound a range argument spells, or `None` when it is no bound.
    pub fn parse(b: &'a [u8]) -> Option<Self> {
        match b {
            b"-" => Some(Self::NegInf),
            b"+" => Some(Self::PosInf),
            [b'[', rest @ ..] => Some(Self::Inclusive(rest)),
            [b'(', rest @ ..] => Some(Self::Exclusive(rest)),
            _ => None,
        }
    }

    /// `m` is at or past this lower bound.
    pub(crate) fn admits_from_below(self, m: &[u8]) -> bool {
        match self {
            Self::NegInf => true,
            Self::PosInf => false,
            Self::Inclusive(b) => m >= b,
            Self::Exclusive(b) => m > b,
        }
    }

    /// `m` is at or before this upper bound.
    pub(crate) fn admits_from_above(self, m: &[u8]) -> bool {
        match self {
            Self::NegInf => false,
            Self::PosInf => true,
            Self::Inclusive(b) => m <= b,
            Self::Exclusive(b) => m < b,
        }
    }
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
        Ok(crate::zset_range::owned(self.zrange_select(
            key,
            ZSpan::Lex(min.as_end(), max.as_end()),
            false,
            None,
        )?))
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
        Ok(self.zrange_select(key, ZSpan::Lex(min.as_end(), max.as_end()), false, None)?.len())
    }

    /// `ZREMRANGEBYLEX` — remove the members within `[min, max]` by bytes;
    /// how many went.
    pub fn zremrange_by_lex(
        &mut self,
        key: &[u8],
        min: &LexBound,
        max: &LexBound,
    ) -> Result<usize, StoreError> {
        self.zremrange_by_lex_ends(key, min.as_end(), max.as_end())
    }

    /// [`Self::zremrange_by_lex`] for borrowed bounds.
    ///
    /// ```
    /// use kevy_store::LexEnd;
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(0.0, b"a".as_slice()), (0.0, b"b"), (0.0, b"c")])?;
    /// assert_eq!(s.zremrange_by_lex_ends(b"z", LexEnd::Inclusive(b"b"), LexEnd::PosInf)?, 2);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zremrange_by_lex_ends(
        &mut self,
        key: &[u8],
        min: LexEnd<'_>,
        max: LexEnd<'_>,
    ) -> Result<usize, StoreError> {
        let gone =
            crate::zset_range::owned(self.zrange_select(key, ZSpan::Lex(min, max), false, None)?);
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
