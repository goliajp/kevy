//! Sorted-set reads and pops from the top end, and random picks:
//! `ZPOPMAX`, `ZREVRANK`, `ZRANDMEMBER`. Same `impl Store` as
//! [`crate::zset`]; split out to keep that file small.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::SmallBytes;
use crate::{Store, StoreError, ZSpan};

impl Store {
    /// `ZPOPMAX` — remove and return up to `count` highest-scored members,
    /// highest first; among equal scores the greater member goes first.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(1.0, b"a"), (3.0, b"c"), (3.0, b"d")])?;
    /// assert_eq!(s.zpopmax(b"z", 2)?, vec![(b"d".to_vec(), 3.0), (b"c".to_vec(), 3.0)]);
    /// assert_eq!(s.zcard(b"z")?, 1);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zpopmax(&mut self, key: &[u8], count: usize) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        let mut top = Vec::new();
        self.zpop_each(key, count, true, |m, s| top.push((m.to_vec(), s)))?;
        Ok(top)
    }

    /// `ZPOPMIN` / `ZPOPMAX` (`max`) of up to `count` members, each handed
    /// to `f` just before it goes; how many went. A wrong-typed key is an
    /// error even for a count of 0.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b"), (3.0, b"c")])?;
    /// let mut got = Vec::new();
    /// assert_eq!(s.zpop_each(b"z", 2, true, |m, sc| got.push((m.to_vec(), sc)))?, 2);
    /// assert_eq!(got, [(b"c".to_vec(), 3.0), (b"b".to_vec(), 2.0)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zpop_each(
        &mut self,
        key: &[u8],
        count: usize,
        max: bool,
        mut f: impl FnMut(&[u8], f64),
    ) -> Result<usize, StoreError> {
        let take = count.min(self.zcard(key)?);
        for _ in 0..take {
            // a short member is held inline while the set lets it go
            let gone = {
                let mut end = self.zrange_select(key, ZSpan::Rank(0, 0), max, None)?;
                let Some((m, sc)) = end.next() else { break };
                f(m, sc);
                SmallBytes::from_slice(m)
            };
            self.zrem(key, &[gone.as_slice()])?;
        }
        Ok(take)
    }

    /// `ZREVRANK` — the member's rank counted from the highest score.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b")])?;
    /// assert_eq!(s.zrevrank(b"z", b"a")?, Some(1));
    /// assert_eq!(s.zrevrank(b"z", b"x")?, None);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zrevrank(&mut self, key: &[u8], member: &[u8]) -> Result<Option<usize>, StoreError> {
        let Some(rank) = self.zrank(key, member)? else {
            return Ok(None);
        };
        Ok(Some(self.zcard(key)? - 1 - rank))
    }

    /// `ZRANDMEMBER key count` — a positive count picks that many distinct
    /// members (all of them when it is at least the size), a negative one
    /// picks `-count` members that may repeat. In random order.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b")])?;
    /// assert_eq!(s.zrandmember(b"z", 5)?.len(), 2);
    /// assert_eq!(s.zrandmember(b"z", -5)?.len(), 5);
    /// assert!(s.zrandmember(b"none", 3)?.is_empty());
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zrandmember(
        &mut self,
        key: &[u8],
        count: i64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        let ranks = self.zrandmember_ranks(key, count)?;
        let mut out = Vec::with_capacity(ranks.len());
        for r in ranks {
            out.extend(
                self.zrange_select(key, ZSpan::Rank(r as i64, r as i64), false, None)?
                    .map(|(m, s)| (m.to_vec(), s)),
            );
        }
        Ok(out)
    }

    /// The ranks [`Self::zrandmember`] picks, in the order it returns
    /// them, for a caller that reads the members in place.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b")])?;
    /// let mut r = s.zrandmember_ranks(b"z", 2)?;
    /// r.sort();
    /// assert_eq!(r, [0, 1]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zrandmember_ranks(&mut self, key: &[u8], count: i64) -> Result<Vec<usize>, StoreError> {
        let n = self.zcard(key)?;
        if n == 0 || count == 0 {
            return Ok(Vec::new());
        }
        let mut ranks: Vec<usize> = if count < 0 {
            (0..count.unsigned_abs()).map(|_| (self.rng.next_u64() % n as u64) as usize).collect()
        } else if count as u64 >= n as u64 {
            (0..n).collect()
        } else {
            self.distinct_ranks(n, count as usize)
        };
        if count > 0 {
            // Fisher–Yates: the distinct picks come back in no set order
            for i in (1..ranks.len()).rev() {
                let j = (self.rng.next_u64() % (i as u64 + 1)) as usize;
                ranks.swap(i, j);
            }
        }
        Ok(ranks)
    }

    /// The rank `ZRANDMEMBER key` picks, as `zrandmember_ranks(key, 1)`
    /// would, without a list for it.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(1.0, b"a")])?;
    /// assert_eq!(s.zrandmember_rank(b"z")?, Some(0));
    /// assert_eq!(s.zrandmember_rank(b"none")?, None);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zrandmember_rank(&mut self, key: &[u8]) -> Result<Option<usize>, StoreError> {
        Ok(match self.zcard(key)? {
            0 => None,
            1 => Some(0),
            n => Some((self.rng.next_u64() % n as u64) as usize),
        })
    }

    /// `k` distinct ranks out of `0..n` (Floyd's sampling).
    fn distinct_ranks(&mut self, n: usize, k: usize) -> Vec<usize> {
        let mut chosen = alloc::collections::BTreeSet::new();
        for j in n - k..n {
            let t = (self.rng.next_u64() % (j as u64 + 1)) as usize;
            if !chosen.insert(t) {
                chosen.insert(j);
            }
        }
        chosen.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

    #[test]
    fn random_picks_are_distinct_members_for_a_positive_count() {
        let mut s = Store::new();
        let members: Vec<Vec<u8>> = (0..50).map(|i| format!("m{i}").into_bytes()).collect();
        let pairs: Vec<(f64, &[u8])> =
            members.iter().enumerate().map(|(i, m)| (i as f64, m.as_slice())).collect();
        s.zadd(b"z", &pairs).unwrap();
        let mut seen_first = std::collections::HashSet::new();
        for _ in 0..200 {
            let got = s.zrandmember(b"z", 7).unwrap();
            assert_eq!(got.len(), 7);
            let distinct: std::collections::HashSet<_> = got.iter().map(|(m, _)| m).collect();
            assert_eq!(distinct.len(), 7, "a positive count never repeats a member");
            for (m, sc) in &got {
                assert_eq!(s.zscore(b"z", m).unwrap(), Some(*sc));
            }
            seen_first.insert(got[0].0.clone());
        }
        assert!(seen_first.len() > 20, "the first pick moves around: {}", seen_first.len());
    }

    #[test]
    fn a_negative_count_may_repeat_and_covers_the_set() {
        let mut s = Store::new();
        s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c")]).unwrap();
        let got = s.zrandmember(b"z", -300).unwrap();
        assert_eq!(got.len(), 300);
        let distinct: std::collections::HashSet<_> = got.iter().map(|(m, _)| m.clone()).collect();
        assert_eq!(distinct.len(), 3);
    }

    #[test]
    fn popping_the_top_empties_the_key_and_checks_its_type() {
        let mut s = Store::new();
        s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b")]).unwrap();
        assert_eq!(s.zpopmax(b"z", 9).unwrap().len(), 2);
        assert!(!s.key_exists(b"z"));
        s.set_slice(b"s", b"v", None, crate::SetCondition::Always);
        assert!(s.zpopmax(b"s", 0).is_err());
        assert_eq!(s.zrevrank(b"z", b"a").unwrap(), None);
    }
}
