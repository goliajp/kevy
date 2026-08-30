//! `Store` set read commands — split from `set.rs` when the SegSet
//! arms pushed it against the 500-LOC cap.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::Value;
use crate::{Store, StoreError};

impl Store {
    /// Membership. A missing key is `false`, not an error; a
    /// wrong-typed key is an error.
    pub fn sismember(&mut self, key: &[u8], member: &[u8]) -> Result<bool, StoreError> {
        match self.live_entry(key) {
            None => Ok(false),
            Some(e) => match &e.value {
                Value::Set(s) => Ok(s.contains(member)),
                Value::SegSet(s) => Ok(s.contains_key(member)),
                Value::SmallSetInline(s) => Ok(s.contains(member)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Member count. A missing key is 0, matching SCARD.
    pub fn scard(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        match self.live_entry(key) {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::Set(s) => Ok(s.len()),
                Value::SegSet(s) => Ok(s.len()),
                Value::SmallSetInline(s) => Ok(s.len()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Every member, copied out. Unordered — a set has no order to
    /// preserve, and callers that need one must sort.
    pub fn smembers(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Set(s) => Ok(s.iter().map(kevy_bytes::SmallBytes::to_vec).collect()),
                Value::SegSet(s) => Ok(s.keys().map(kevy_bytes::SmallBytes::to_vec).collect()),
                Value::SmallSetInline(s) => Ok(s.iter_slices().map(<[u8]>::to_vec).collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `SRANDMEMBER key count` — up to `count` DISTINCT arbitrary
    /// members, not removed.
    ///
    /// Two regimes, as Redis has: when `count` is a small fraction of
    /// the set, probe random slots and reject duplicates — O(count)
    /// expected. When it is most of the set, rejection would thrash, so
    /// copy the members out and shuffle a prefix instead.
    pub fn srandmember(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut draws: Vec<u64> =
            (0..count.saturating_mul(3).max(8)).map(|_| self.rng.next_u64()).collect();
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::SmallSetInline(s) => {
                    let mut all: Vec<Vec<u8>> = s.iter_slices().map(<[u8]>::to_vec).collect();
                    let k = crate::set::shuffle_prefix(&mut all, count, &mut draws);
                    all.truncate(k);
                    Ok(all)
                }
                Value::Set(s) => {
                    let n = s.len();
                    if count >= n {
                        return Ok(s.iter().map(kevy_bytes::SmallBytes::to_vec).collect());
                    }
                    if count * 4 >= n {
                        // Wanting most of the set: copying beats rejecting.
                        let mut all: Vec<Vec<u8>> =
                            s.iter().map(kevy_bytes::SmallBytes::to_vec).collect();
                        let k = crate::set::shuffle_prefix(&mut all, count, &mut draws);
                        all.truncate(k);
                        return Ok(all);
                    }
                    let mut out: Vec<Vec<u8>> = Vec::with_capacity(count);
                    for slot in &draws {
                        if out.len() == count {
                            break;
                        }
                        if let Some(m) = s
                            .iter_from_slot(*slot as usize)
                            .next()
                            .map(kevy_bytes::SmallBytes::to_vec)
                            && !out.contains(&m)
                        {
                            out.push(m);
                        }
                    }
                    Ok(out)
                }
                Value::SegSet(s) => Ok(seg_srandmember(s, count, &mut draws)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `SRANDMEMBER key -count` — exactly `count` members, WITH
    /// repetition.
    pub fn srandmember_with_repeats(
        &mut self,
        key: &[u8],
        count: usize,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        let draws: Vec<u64> = (0..count).map(|_| self.rng.next_u64()).collect();
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::SmallSetInline(s) => {
                    let all: Vec<Vec<u8>> = s.iter_slices().map(<[u8]>::to_vec).collect();
                    if all.is_empty() {
                        return Ok(Vec::new());
                    }
                    Ok(draws.iter().map(|d| all[(*d as usize) % all.len()].clone()).collect())
                }
                Value::Set(s) => {
                    if s.is_empty() {
                        return Ok(Vec::new());
                    }
                    Ok(draws
                        .iter()
                        .filter_map(|d| {
                            s.iter_from_slot(*d as usize).next().map(kevy_bytes::SmallBytes::to_vec)
                        })
                        .collect())
                }
                Value::SegSet(s) => {
                    if s.is_empty() {
                        return Ok(Vec::new());
                    }
                    Ok(draws
                        .iter()
                        .filter_map(|d| s.rand_entry(*d).map(|(m, ())| m.to_vec()))
                        .collect())
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Snapshot of a set's members for cross-shard algebra (SINTER/etc.).
    pub fn set_snapshot(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.smembers(key)
    }
}

/// SRANDMEMBER over a sharded set: rejection-probe via the weighted
/// random walk; degenerate huge counts fall back to the copy regime
/// like the flat path.
fn seg_srandmember(
    s: &crate::seg_map::SegMap<()>,
    count: usize,
    draws: &mut Vec<u64>,
) -> Vec<Vec<u8>> {
    if count * 4 >= s.len() {
        let mut all: Vec<Vec<u8>> = s.keys().map(kevy_bytes::SmallBytes::to_vec).collect();
        let k = crate::set::shuffle_prefix(&mut all, count, draws);
        all.truncate(k);
        return all;
    }
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(count);
    for d in draws.iter() {
        if out.len() == count {
            break;
        }
        if let Some((m, ())) = s.rand_entry(*d)
            && !out.contains(&m.to_vec())
        {
            out.push(m.to_vec());
        }
    }
    out
}
