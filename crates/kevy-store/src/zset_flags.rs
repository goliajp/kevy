//! `ZADD` condition flags (Redis 6.2): `NX` / `XX` / `GT` / `LT` /
//! `CH` / `INCR`. Split from `zset.rs` (500-LOC rule). The no-flags
//! hot path stays `zadd` / `zadd` — nothing here taxes it.

use crate::{Store, StoreError};

/// Parsed `ZADD` condition flags. `CH` only changes the *reply*
/// (changed count instead of added count) — callers read
/// [`ZaddReport::changed`] when set; the engine behavior is identical.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZaddFlags {
    /// Only add new members; never update existing ones.
    pub nx: bool,
    /// Only update existing members; never add new ones.
    pub xx: bool,
    /// Only update when the new score is greater than the current.
    pub gt: bool,
    /// Only update when the new score is less than the current.
    pub lt: bool,
    /// Reply with changed (added + updated) instead of added.
    pub ch: bool,
}

impl ZaddFlags {
    /// Redis 6.2 rule: `GT`, `LT` and `NX` are mutually exclusive
    /// (and `GT`+`LT` together are, too). `XX`+`NX` likewise.
    pub fn valid(self) -> bool {
        !(self.nx && (self.xx || self.gt || self.lt)) && !(self.gt && self.lt)
    }
}

/// Outcome of a flags-aware `ZADD`.
pub struct ZaddReport {
    /// Members newly added.
    pub added: usize,
    /// Members added or whose score actually changed (`CH` reply).
    pub changed: usize,
    /// The `(score, member)` pairs actually applied, in input order —
    /// vetoed pairs are absent. Lets an AOF writer log the *effect*
    /// as a plain unconditional `ZADD` (deterministic on replay; a
    /// conditional replayed against divergent state could veto
    /// differently).
    pub applied: Vec<(f64, Vec<u8>)>,
}

impl Store {
    /// Flags-aware `ZADD`. Caller validates [`ZaddFlags::valid`] at
    /// its input boundary (RESP parse / typed API) — invalid combos
    /// here are a caller bug.
    pub fn zadd_flags(
        &mut self,
        key: &[u8],
        pairs: &[(f64, &[u8])],
        flags: ZaddFlags,
    ) -> Result<ZaddReport, StoreError> {
        debug_assert!(flags.valid(), "caller must reject invalid flag combos");
        let mut rep = ZaddReport { added: 0, changed: 0, applied: Vec::new() };
        for (score, m) in pairs {
            match self.zscore(key, m)? {
                Some(old) => {
                    if flags.nx
                        || (flags.gt && *score <= old)
                        || (flags.lt && *score >= old)
                    {
                        continue;
                    }
                    if *score != old {
                        self.zadd(key, &[(*score, m)])?;
                        rep.changed += 1;
                        rep.applied.push((*score, m.to_vec()));
                    }
                }
                None => {
                    if flags.xx {
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
        debug_assert!(flags.valid(), "caller must reject invalid flag combos");
        match self.zscore(key, member)? {
            Some(old) => {
                if flags.nx {
                    return Ok(None);
                }
                let next = old + delta;
                if !next.is_finite() {
                    return Err(StoreError::NotFloat);
                }
                if (flags.gt && next <= old) || (flags.lt && next >= old) {
                    return Ok(None);
                }
                self.zadd(key, &[(next, member)])?;
                Ok(Some(next))
            }
            None => {
                if flags.xx {
                    return Ok(None);
                }
                if !delta.is_finite() {
                    return Err(StoreError::NotFloat);
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

    #[test]
    fn validity_matrix() {
        assert!(zf().valid());
        assert!(ZaddFlags { gt: true, ch: true, ..zf() }.valid());
        assert!(ZaddFlags { xx: true, gt: true, ..zf() }.valid());
        assert!(!ZaddFlags { nx: true, xx: true, ..zf() }.valid());
        assert!(!ZaddFlags { nx: true, gt: true, ..zf() }.valid());
        assert!(!ZaddFlags { gt: true, lt: true, ..zf() }.valid());
    }

    #[test]
    fn nx_only_adds() {
        let mut s = Store::new();
        s.zadd(b"z", &[(1.0, b"m".as_slice())]).unwrap();
        let r = s
            .zadd_flags(b"z", &[(9.0, b"m"), (2.0, b"n")], ZaddFlags { nx: true, ..zf() })
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
            .zadd_flags(b"z", &[(9.0, b"m"), (2.0, b"n")], ZaddFlags { xx: true, ..zf() })
            .unwrap();
        assert_eq!((r.added, r.changed), (0, 1));
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(9.0));
        assert_eq!(s.zscore(b"z", b"n").unwrap(), None); // not added
    }

    #[test]
    fn gt_is_monotonic_heal() {
        let mut s = Store::new();
        s.zadd(b"z", &[(5.0, b"m".as_slice())]).unwrap();
        let gt = ZaddFlags { gt: true, ..zf() };
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
        let lt = ZaddFlags { lt: true, ..zf() };
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
                ZaddFlags { gt: true, ..zf() },
            )
            .unwrap();
        // a updated, b vetoed, c added.
        assert_eq!(
            r.applied,
            vec![(9.0, b"a".to_vec()), (5.0, b"c".to_vec())]
        );
    }

    #[test]
    fn incr_form_vetoes_to_none() {
        let mut s = Store::new();
        s.zadd(b"z", &[(5.0, b"m".as_slice())]).unwrap();
        let gt = ZaddFlags { gt: true, ..zf() };
        // Negative delta under GT: next < old → nil, score untouched.
        assert_eq!(s.zadd_incr(b"z", -2.0, b"m", gt).unwrap(), None);
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(5.0));
        assert_eq!(s.zadd_incr(b"z", 2.0, b"m", gt).unwrap(), Some(7.0));
        // XX on a missing member → nil.
        let xx = ZaddFlags { xx: true, ..zf() };
        assert_eq!(s.zadd_incr(b"z", 1.0, b"nope", xx).unwrap(), None);
        // NX on an existing member → nil.
        let nx = ZaddFlags { nx: true, ..zf() };
        assert_eq!(s.zadd_incr(b"z", 1.0, b"m", nx).unwrap(), None);
    }

    #[test]
    fn wrongtype_propagates() {
        let mut s = Store::new();
        s.set(b"str", b"v".to_vec(), None, false, false);
        assert!(s.zadd_flags(b"str", &[(1.0, b"m")], zf()).is_err());
        assert!(s.zadd_incr(b"str", 1.0, b"m", zf()).is_err());
    }
}
