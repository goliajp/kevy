//! zset algebra (Redis 6.2 `ZINTERSTORE` / `ZUNIONSTORE` /
//! `ZDIFFSTORE` / `ZINTERCARD` semantics): pure combination helpers
//! over gathered `(member, score)` lists + the store-side
//! materialization write.
//!
//! The pure functions take inputs ALREADY extracted from their keys
//! (each input = one source key's scored members, sets contributing
//! score 1.0 per Redis) so both consumers share one semantics:
//! the embedded facade (reads under shard locks) and the server's
//! cross-shard gather reducer.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::{Store, StoreError};

/// Scratch tables for the set-algebra passes: std's hash tables on
/// `std`, alloc's B-trees without it (`no_std` correctness form —
/// these are per-call temporaries, not hot state).
#[cfg(feature = "std")]
type ScratchMap<K, V> = std::collections::HashMap<K, V>;
#[cfg(feature = "std")]
type ScratchSet<T> = std::collections::HashSet<T>;
#[cfg(not(feature = "std"))]
type ScratchMap<K, V> = alloc::collections::BTreeMap<K, V>;
#[cfg(not(feature = "std"))]
type ScratchSet<T> = alloc::collections::BTreeSet<T>;

/// `AGGREGATE` mode for inter/union (Redis 6.2; default `Sum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZAggregate {
    /// Weighted sum of scores (the default).
    Sum,
    /// Minimum weighted score.
    Min,
    /// Maximum weighted score.
    Max,
}

fn agg(a: f64, b: f64, mode: ZAggregate) -> f64 {
    match mode {
        ZAggregate::Sum => a + b,
        ZAggregate::Min => a.min(b),
        ZAggregate::Max => a.max(b),
    }
}

fn weight_of(weights: Option<&[f64]>, i: usize) -> f64 {
    weights.map_or(1.0, |w| w.get(i).copied().unwrap_or(1.0))
}

/// `ZUNIONSTORE` combination: every member of any input, scores
/// aggregated across the inputs it appears in (weighted).
pub fn zunion(
    inputs: &[Vec<(Vec<u8>, f64)>],
    weights: Option<&[f64]>,
    mode: ZAggregate,
) -> Vec<(Vec<u8>, f64)> {
    let mut acc: Vec<(Vec<u8>, f64)> = Vec::new();
    let mut idx: ScratchMap<Vec<u8>, usize> = ScratchMap::new();
    for (i, input) in inputs.iter().enumerate() {
        let w = weight_of(weights, i);
        for (m, s) in input {
            let ws = s * w;
            match idx.get(m) {
                Some(&slot) => acc[slot].1 = agg(acc[slot].1, ws, mode),
                None => {
                    idx.insert(m.clone(), acc.len());
                    acc.push((m.clone(), ws));
                }
            }
        }
    }
    acc
}

/// `ZINTERSTORE` combination: members present in EVERY input.
pub fn zinter(
    inputs: &[Vec<(Vec<u8>, f64)>],
    weights: Option<&[f64]>,
    mode: ZAggregate,
) -> Vec<(Vec<u8>, f64)> {
    let Some((first, rest)) = inputs.split_first() else {
        return Vec::new();
    };
    // Membership maps for the non-first inputs.
    let maps: Vec<ScratchMap<&[u8], f64>> = rest
        .iter()
        .map(|inp| inp.iter().map(|(m, s)| (m.as_slice(), *s)).collect())
        .collect();
    let w0 = weight_of(weights, 0);
    let mut out = Vec::new();
    'member: for (m, s) in first {
        let mut score = s * w0;
        for (j, map) in maps.iter().enumerate() {
            match map.get(m.as_slice()) {
                Some(&sj) => score = agg(score, sj * weight_of(weights, j + 1), mode),
                None => continue 'member,
            }
        }
        out.push((m.clone(), score));
    }
    out
}

/// `ZDIFFSTORE` combination: members of the first input absent from
/// every other input; scores from the first input (no weights /
/// aggregate — Redis 6.2 defines none for ZDIFF).
pub fn zdiff(inputs: &[Vec<(Vec<u8>, f64)>]) -> Vec<(Vec<u8>, f64)> {
    let Some((first, rest)) = inputs.split_first() else {
        return Vec::new();
    };
    let excluded: ScratchSet<&[u8]> = rest
        .iter()
        .flat_map(|inp| inp.iter().map(|(m, _)| m.as_slice()))
        .collect();
    first
        .iter()
        .filter(|(m, _)| !excluded.contains(m.as_slice()))
        .cloned()
        .collect()
}

/// `ZINTERCARD` (with optional `LIMIT`, 0 = unlimited): cardinality of
/// the intersection, short-circuiting at the limit.
pub fn zintercard(inputs: &[Vec<(Vec<u8>, f64)>], limit: usize) -> usize {
    let Some((first, rest)) = inputs.split_first() else {
        return 0;
    };
    let maps: Vec<ScratchSet<&[u8]>> = rest
        .iter()
        .map(|inp| inp.iter().map(|(m, _)| m.as_slice()).collect())
        .collect();
    let mut n = 0;
    'member: for (m, _) in first {
        for map in &maps {
            if !map.contains(m.as_slice()) {
                continue 'member;
            }
        }
        n += 1;
        if limit != 0 && n >= limit {
            return n;
        }
    }
    n
}

impl Store {
    /// Extract one source key's scored members for the algebra ops:
    /// zsets as-is, sets with score 1.0 (Redis semantics), absent key
    /// = empty, any other type = `WrongType`.
    pub fn zset_or_set_members(&mut self, key: &[u8]) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        use crate::Value;
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::ZSet(z) => Ok(z
                    .by_member
                    .iter()
                    .map(|(m, s)| (m.to_vec(), *s))
                    .collect()),
                Value::SmallZSetInline(z) => {
                    Ok(z.iter().map(|(m, s)| (m.to_vec(), s)).collect())
                }
                Value::Set(s) => Ok(s.iter().map(|m| (m.to_vec(), 1.0)).collect()),
                Value::SegSet(s) => Ok(s.keys().map(|m| (m.to_vec(), 1.0)).collect()),
                Value::SmallSetInline(s) => Ok(s.iter().map(|m| (m.to_vec(), 1.0)).collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Materialize an algebra result at `dst`: existing value (any
    /// type) is dropped, result written as a zset — Redis `*STORE`
    /// overwrite semantics. Empty result deletes `dst` (Redis drops
    /// the destination rather than storing an empty zset). Returns
    /// the stored cardinality.
    pub fn zstore_result(&mut self, dst: &[u8], pairs: &[(Vec<u8>, f64)]) -> usize {
        self.del(&[dst]);
        if pairs.is_empty() {
            return 0;
        }
        let scored: Vec<(f64, &[u8])> = pairs.iter().map(|(m, s)| (*s, m.as_slice())).collect();
        let _ = self.zadd(dst, &scored);
        pairs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zs(pairs: &[(&str, f64)]) -> Vec<(Vec<u8>, f64)> {
        pairs.iter().map(|(m, s)| (m.as_bytes().to_vec(), *s)).collect()
    }

    #[test]
    fn union_weights_and_aggregates() {
        let a = zs(&[("x", 1.0), ("y", 2.0)]);
        let b = zs(&[("y", 3.0), ("z", 4.0)]);
        let mut u = zunion(&[a.clone(), b.clone()], None, ZAggregate::Sum);
        u.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(u, zs(&[("x", 1.0), ("y", 5.0), ("z", 4.0)]));
        // WEIGHTS 2 3, MIN
        let mut u = zunion(&[a, b], Some(&[2.0, 3.0]), ZAggregate::Min);
        u.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(u, zs(&[("x", 2.0), ("y", 4.0), ("z", 12.0)]));
    }

    #[test]
    fn inter_semantics() {
        let a = zs(&[("x", 1.0), ("y", 2.0)]);
        let b = zs(&[("y", 3.0), ("z", 4.0)]);
        assert_eq!(zinter(&[a.clone(), b.clone()], None, ZAggregate::Sum), zs(&[("y", 5.0)]));
        assert_eq!(
            zinter(&[a, b], Some(&[10.0, 1.0]), ZAggregate::Max),
            zs(&[("y", 20.0)])
        );
        assert!(zinter(&[], None, ZAggregate::Sum).is_empty());
    }

    #[test]
    fn diff_and_intercard() {
        let a = zs(&[("x", 1.0), ("y", 2.0), ("z", 3.0)]);
        let b = zs(&[("y", 9.0)]);
        let mut d = zdiff(&[a.clone(), b.clone()]);
        d.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(d, zs(&[("x", 1.0), ("z", 3.0)]));
        assert_eq!(zintercard(&[a.clone(), b.clone()], 0), 1);
        let c = zs(&[("x", 0.0), ("y", 0.0), ("z", 0.0)]);
        assert_eq!(zintercard(&[a.clone(), c.clone()], 0), 3);
        assert_eq!(zintercard(&[a, c], 2), 2); // LIMIT short-circuit
    }

    #[test]
    fn store_materialization_and_source_extraction() {
        let mut s = Store::new();
        s.zadd(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b".as_slice())]).unwrap();
        s.sadd(b"s", &[b"a".as_slice(), b"c".as_slice()]).unwrap();
        let mut zm = s.zset_or_set_members(b"z").unwrap();
        zm.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(zm, zs(&[("a", 1.0), ("b", 2.0)]));
        let mut sm = s.zset_or_set_members(b"s").unwrap();
        sm.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(sm, zs(&[("a", 1.0), ("c", 1.0)]));
        assert!(s.zset_or_set_members(b"missing").unwrap().is_empty());
        s.set(b"str", b"v".to_vec(), None, false, false);
        assert!(s.zset_or_set_members(b"str").is_err());

        // *STORE overwrite + empty-result-deletes semantics.
        s.set(b"dst", b"old".to_vec(), None, false, false);
        assert_eq!(s.zstore_result(b"dst", &zs(&[("m", 7.0)])), 1);
        assert_eq!(s.zscore(b"dst", b"m").unwrap(), Some(7.0));
        assert_eq!(s.zstore_result(b"dst", &[]), 0);
        assert_eq!(s.zcard(b"dst").unwrap(), 0);
        assert_eq!(s.exists(&[b"dst".as_slice()]), 0);
    }
}
