//! `ZADD` condition flags (Redis 6.2): `NX` / `XX` / `GT` / `LT` /
//! `CH` / `INCR`. Split from `zset.rs` (500-LOC rule). The no-flags
//! hot path stays `zadd` / `zadd` — nothing here taxes it.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::{ScoreCompare, SetCondition, Store, StoreError};

/// Parsed `ZADD` condition flags: `NX` / `XX` as a [`SetCondition`],
/// `GT` / `LT` as a [`ScoreCompare`], and `CH`. Only Redis's legal
/// combinations can be built — [`ZaddFlags::new`] refuses `NX` with
/// `GT` / `LT` — so the store never has a combination to reject. `CH`
/// only changes the *reply* (changed count instead of added count);
/// [`ZaddReport`] carries both counts either way.
///
/// ```
/// use kevy_store::{ScoreCompare, SetCondition, ZaddFlags};
/// let f = ZaddFlags::new(SetCondition::IfPresent, ScoreCompare::Greater).unwrap().with_ch(true);
/// assert_eq!((f.condition(), f.compare(), f.ch()), (SetCondition::IfPresent, ScoreCompare::Greater, true));
/// assert!(ZaddFlags::new(SetCondition::IfAbsent, ScoreCompare::Less).is_none());
/// assert_eq!(ZaddFlags::default(), ZaddFlags::new(SetCondition::Always, ScoreCompare::Any).unwrap());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ZaddFlags {
    condition: SetCondition,
    compare: ScoreCompare,
    ch: bool,
}

impl ZaddFlags {
    /// The flags for `condition` and `compare`, or `None` for the one
    /// combination Redis refuses: `NX` together with `GT` or `LT`.
    pub fn new(condition: SetCondition, compare: ScoreCompare) -> Option<Self> {
        let legal = condition != SetCondition::IfAbsent || compare == ScoreCompare::Any;
        legal.then_some(Self { condition, compare, ch: false })
    }

    /// Set `CH`: the reply counts changed members instead of added ones.
    ///
    /// ```
    /// assert!(kevy_store::ZaddFlags::default().with_ch(true).ch());
    /// ```
    #[must_use]
    pub fn with_ch(mut self, ch: bool) -> Self {
        self.ch = ch;
        self
    }

    /// `NX` / `XX`, or neither.
    pub fn condition(self) -> SetCondition {
        self.condition
    }

    /// `GT` / `LT`, or neither.
    pub fn compare(self) -> ScoreCompare {
        self.compare
    }

    /// Whether `CH` was given.
    pub fn ch(self) -> bool {
        self.ch
    }

    /// Whether these flags veto replacing an existing member's `old`
    /// score with `new`.
    fn vetoes_update(self, old: f64, new: f64) -> bool {
        self.condition == SetCondition::IfAbsent
            || match self.compare {
                ScoreCompare::Greater => new <= old,
                ScoreCompare::Less => new >= old,
                ScoreCompare::Any => false,
            }
    }
}

/// Outcome of a flags-aware `ZADD`.
///
/// ```
/// use kevy_store::{ScoreCompare, SetCondition, Store, ZaddFlags};
/// let mut s = Store::new();
/// s.zadd(b"z", &[(5.0, b"a".as_slice())])?;
/// let gt = ZaddFlags::new(SetCondition::Always, ScoreCompare::Greater).unwrap();
/// let r = s.zadd_flags(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b")], gt)?;
/// assert_eq!((r.added, r.changed), (1, 1)); // `a` vetoed, `b` added
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ZaddReport {
    /// Members newly added.
    ///
    /// ```
    /// use kevy_store::{Store, ZaddFlags};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice())])?;
    /// let r = s.zadd_flags(b"z", &[(2.0, b"a".as_slice()), (1.0, b"b")], ZaddFlags::default())?;
    /// assert_eq!(r.added, 1); // only `b` is new
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub added: usize,
    /// Members added or whose score actually changed (`CH` reply).
    ///
    /// ```
    /// use kevy_store::{Store, ZaddFlags};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice()), (1.0, b"c")])?;
    /// let pairs = [(2.0, b"a".as_slice()), (1.0, b"b"), (1.0, b"c")];
    /// let r = s.zadd_flags(b"z", &pairs, ZaddFlags::default().with_ch(true))?;
    /// assert_eq!(r.changed, 2); // `a` moved, `b` added, `c` unchanged
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub changed: usize,
    /// The `(score, member)` pairs actually applied, in input order —
    /// vetoed pairs are absent. Lets an AOF writer log the *effect*
    /// as a plain unconditional `ZADD` (deterministic on replay; a
    /// conditional replayed against divergent state could veto
    /// differently).
    ///
    /// ```
    /// use kevy_store::{ScoreCompare, SetCondition, Store, ZaddFlags};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(5.0, b"a".as_slice())])?;
    /// let lt = ZaddFlags::new(SetCondition::Always, ScoreCompare::Less).unwrap();
    /// let r = s.zadd_flags(b"z", &[(9.0, b"a".as_slice()), (3.0, b"b")], lt)?;
    /// assert_eq!(r.applied, [(3.0, b"b".to_vec())]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub applied: Vec<(f64, Vec<u8>)>,
}

impl Store {
    /// Flags-aware `ZADD`: each pair is added or updated as `flags`
    /// allow, and the report says what actually happened.
    pub fn zadd_flags(
        &mut self,
        key: &[u8],
        pairs: &[(f64, &[u8])],
        flags: ZaddFlags,
    ) -> Result<ZaddReport, StoreError> {
        let mut rep = ZaddReport::default();
        for (score, m) in pairs {
            match self.zscore(key, m)? {
                Some(old) => {
                    if flags.vetoes_update(old, *score) {
                        continue;
                    }
                    if *score != old {
                        self.zadd(key, &[(*score, m)])?;
                        rep.changed += 1;
                        rep.applied.push((*score, m.to_vec()));
                    }
                }
                None => {
                    if flags.condition == SetCondition::IfPresent {
                        continue;
                    }
                    self.zadd(key, &[(*score, m)])?;
                    rep.added += 1;
                    rep.changed += 1;
                    rep.applied.push((*score, m.to_vec()));
                }
            }
        }
        Ok(rep)
    }

    /// `ZADD … INCR` — a conditional `ZINCRBY`: returns the new score,
    /// or `None` when the flags veto the operation (Redis replies nil).
    pub fn zadd_incr(
        &mut self,
        key: &[u8],
        delta: f64,
        member: &[u8],
        flags: ZaddFlags,
    ) -> Result<Option<f64>, StoreError> {
        match self.zscore(key, member)? {
            Some(old) => {
                if flags.condition == SetCondition::IfAbsent {
                    return Ok(None);
                }
                let next = old + delta;
                if next.is_nan() {
                    return Err(StoreError::ScoreIsNan);
                }
                if flags.vetoes_update(old, next) {
                    return Ok(None);
                }
                self.zadd(key, &[(next, member)])?;
                Ok(Some(next))
            }
            None => {
                if flags.condition == SetCondition::IfPresent {
                    return Ok(None);
                }
                self.zadd(key, &[(delta, member)])?;
                Ok(Some(delta))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zf() -> ZaddFlags {
        ZaddFlags::default()
    }

    fn flags(c: SetCondition, s: ScoreCompare) -> ZaddFlags {
        ZaddFlags::new(c, s).unwrap()
    }

    #[test]
    fn only_nx_with_a_comparison_is_refused() {
        use ScoreCompare::{Any, Greater, Less};
        use SetCondition::{Always, IfAbsent, IfPresent};
        for c in [Always, IfAbsent, IfPresent] {
            for s in [Any, Greater, Less] {
                let refused = c == IfAbsent && s != Any;
                assert_eq!(ZaddFlags::new(c, s).is_none(), refused, "{c:?} {s:?}");
            }
        }
    }

    #[test]
    fn nx_only_adds() {
        let mut s = Store::new();
        s.zadd(b"z", &[(1.0, b"m".as_slice())]).unwrap();
        let r = s
            .zadd_flags(
                b"z",
                &[(9.0, b"m"), (2.0, b"n")],
                flags(SetCondition::IfAbsent, ScoreCompare::Any),
            )
            .unwrap();
        assert_eq!((r.added, r.changed), (1, 1));
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(1.0)); // untouched
        assert_eq!(s.zscore(b"z", b"n").unwrap(), Some(2.0));
    }

    #[test]
    fn xx_only_updates() {
        let mut s = Store::new();
        s.zadd(b"z", &[(1.0, b"m".as_slice())]).unwrap();
        let r = s
            .zadd_flags(
                b"z",
                &[(9.0, b"m"), (2.0, b"n")],
                flags(SetCondition::IfPresent, ScoreCompare::Any),
            )
            .unwrap();
        assert_eq!((r.added, r.changed), (0, 1));
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(9.0));
        assert_eq!(s.zscore(b"z", b"n").unwrap(), None); // not added
    }

    #[test]
    fn gt_is_monotonic_heal() {
        let mut s = Store::new();
        s.zadd(b"z", &[(5.0, b"m".as_slice())]).unwrap();
        let gt = flags(SetCondition::Always, ScoreCompare::Greater);
        // Stale (lower) score: vetoed.
        let r = s.zadd_flags(b"z", &[(3.0, b"m")], gt).unwrap();
        assert_eq!(r.changed, 0);
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(5.0));
        // Newer (higher) score: applied.
        let r = s.zadd_flags(b"z", &[(7.0, b"m")], gt).unwrap();
        assert_eq!(r.changed, 1);
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(7.0));
        // GT still ADDS missing members (only XX suppresses adds).
        let r = s.zadd_flags(b"z", &[(1.0, b"new")], gt).unwrap();
        assert_eq!(r.added, 1);
    }

    #[test]
    fn lt_mirror() {
        let mut s = Store::new();
        s.zadd(b"z", &[(5.0, b"m".as_slice())]).unwrap();
        let lt = flags(SetCondition::Always, ScoreCompare::Less);
        assert_eq!(s.zadd_flags(b"z", &[(7.0, b"m")], lt).unwrap().changed, 0);
        assert_eq!(s.zadd_flags(b"z", &[(3.0, b"m")], lt).unwrap().changed, 1);
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(3.0));
    }

    #[test]
    fn applied_reflects_effect_only() {
        let mut s = Store::new();
        s.zadd(b"z", &[(5.0, b"a".as_slice()), (5.0, b"b".as_slice())]).unwrap();
        let r = s
            .zadd_flags(
                b"z",
                &[(9.0, b"a"), (1.0, b"b"), (5.0, b"c")],
                flags(SetCondition::Always, ScoreCompare::Greater),
            )
            .unwrap();
        // a updated, b vetoed, c added.
        assert_eq!(r.applied, vec![(9.0, b"a".to_vec()), (5.0, b"c".to_vec())]);
    }

    #[test]
    fn incr_form_vetoes_to_none() {
        let mut s = Store::new();
        s.zadd(b"z", &[(5.0, b"m".as_slice())]).unwrap();
        let gt = flags(SetCondition::Always, ScoreCompare::Greater);
        // Negative delta under GT: next < old → nil, score untouched.
        assert_eq!(s.zadd_incr(b"z", -2.0, b"m", gt).unwrap(), None);
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(5.0));
        assert_eq!(s.zadd_incr(b"z", 2.0, b"m", gt).unwrap(), Some(7.0));
        // XX on a missing member → nil.
        let xx = flags(SetCondition::IfPresent, ScoreCompare::Any);
        assert_eq!(s.zadd_incr(b"z", 1.0, b"nope", xx).unwrap(), None);
        // NX on an existing member → nil.
        let nx = flags(SetCondition::IfAbsent, ScoreCompare::Any);
        assert_eq!(s.zadd_incr(b"z", 1.0, b"m", nx).unwrap(), None);
    }

    #[test]
    fn wrongtype_propagates() {
        let mut s = Store::new();
        s.set(b"str", b"v".to_vec(), None, SetCondition::Always);
        assert!(s.zadd_flags(b"str", &[(1.0, b"m")], zf()).is_err());
        assert!(s.zadd_incr(b"str", 1.0, b"m", zf()).is_err());
    }
}
