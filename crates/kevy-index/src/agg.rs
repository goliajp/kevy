//! [`AggSegment`] — one shard's slice of one aggregate index (KIND
//! agg): per-group count / sum / min / max maintained
//! synchronously with writes. min/max stay EXACT under deletion via a
//! per-group value multiset (BTreeMap value → multiplicity); a
//! row → (group, value) reverse map supports O(log) updates — the
//! same derived-by-construction discipline as [`crate::Segment`].

use std::collections::{BTreeMap, HashMap};

use crate::IndexValue;

#[path = "agg_stats.rs"]
mod stats;
pub use stats::{AggBy, GroupStats, sort_groups};

/// What one row contributes to an aggregate index, as
/// [`AggSegment::apply`] takes it.
///
/// ```
/// use kevy_index::{AggRow, AggSegment, IndexValue};
/// let mut s = AggSegment::new();
/// s.apply(b"o:1", AggRow::Member { group: b"paid".to_vec(), value: IndexValue::I64(30) });
/// s.apply(b"o:2", AggRow::Excluded);
/// assert_eq!((s.stats().rows, s.stats().excluded), (1, 1));
/// s.apply(b"o:1", AggRow::Removed);
/// assert_eq!(s.rows(), 0);
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AggRow {
    /// The row participates: in `group`, contributing `value`.
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// assert_eq!(s.group(b"eu").count, 2);
    /// // re-applying a key moves the row to its new group
    /// s.apply(b"o:2", row(b"us", 12));
    /// assert_eq!((s.group(b"eu").count, s.group(b"us").count), (1, 1));
    /// ```
    Member {
        /// The grouping field's raw bytes.
        ///
        /// ```
        /// use kevy_index::{AggRow, AggSegment, IndexValue};
        /// let mut s = AggSegment::new();
        /// s.apply(b"o:1", AggRow::Member { group: b"eu".to_vec(), value: IndexValue::I64(1) });
        /// assert_eq!(s.all_groups()[0].0, b"eu");
        /// ```
        group: Vec<u8>,
        /// The aggregated field, coerced to the index's type.
        ///
        /// ```
        /// use kevy_index::{AggRow, AggSegment, IndexValue, ValType};
        /// let mut s = AggSegment::new();
        /// let value = IndexValue::coerce(ValType::F64, b"2.5").expect("a number");
        /// s.apply(b"o:1", AggRow::Member { group: b"eu".to_vec(), value });
        /// assert_eq!(s.group(b"eu").sum, 2.5);
        /// ```
        value: IndexValue,
    },
    /// The row is gone (deleted, or moved out of the prefix).
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// s.apply(b"o:1", AggRow::Removed);
    /// s.apply(b"o:9", AggRow::Removed); // an unknown key is a no-op
    /// assert!(!s.contains(b"o:1"));
    /// assert_eq!((s.rows(), s.stats().excluded), (1, 0));
    /// ```
    Removed,
    /// The row exists but cannot participate: its grouping field is
    /// missing or its value failed coercion. Counted, unlike a removal.
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// s.apply(b"o:1", AggRow::Excluded);
    /// assert_eq!(s.group(b"eu").count, 1, "its contribution is retracted");
    /// assert_eq!(s.stats().excluded, 1);
    /// ```
    Excluded,
}

#[derive(Debug)]
struct Group {
    count: u64,
    sum: f64,
    /// value → multiplicity; min/max = first/last key.
    values: BTreeMap<IndexValue, u32>,
}

/// Sizing counters (memory formula / IDX.LIST).
///
/// ```
/// use kevy_index::{AggRow, AggSegment, IndexValue};
/// let mut s = AggSegment::new();
/// assert_eq!(s.stats().approx_bytes, 0);
/// s.apply(b"o:1", AggRow::Member { group: b"eu".to_vec(), value: IndexValue::I64(1) });
/// assert!(s.stats().approx_bytes > 0);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct AggStats {
    /// Live groups.
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// s.apply(b"o:3", row(b"us", 1));
    /// assert_eq!(s.stats().groups, 2);
    /// ```
    pub groups: u64,
    /// Rows participating.
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// s.apply(b"o:3", AggRow::Excluded);
    /// assert_eq!(s.stats().rows, 2, "an excluded row does not participate");
    /// ```
    pub rows: u64,
    /// Rows excluded (coerce failure / missing group field).
    ///
    /// ```
    /// use kevy_index::{AggRow, AggSegment, IndexValue, ValType};
    /// let mut s = AggSegment::new();
    /// let row = match IndexValue::coerce(ValType::I64, b"n/a") {
    ///     Some(value) => AggRow::Member { group: b"eu".to_vec(), value },
    ///     None => AggRow::Excluded,
    /// };
    /// s.apply(b"o:1", row);
    /// assert_eq!(s.stats().excluded, 1);
    /// ```
    pub excluded: u64,
    /// Approximate heap bytes (the measured side of the documented
    /// memory formula).
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// let two = s.stats().approx_bytes;
    /// s.apply(b"o:3", row(b"us", 7));
    /// assert!(s.stats().approx_bytes > two, "a new group and row cost bytes");
    /// ```
    pub approx_bytes: u64,
}

/// One shard's aggregate segment.
///
/// ```
/// use kevy_index::{AggBy, AggRow, AggSegment, IndexValue};
/// let mut s = AggSegment::new();
/// let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
/// s.apply(b"o:1", row(b"eu", 30));
/// s.apply(b"o:2", row(b"eu", 12));
/// s.apply(b"o:3", row(b"us", 50));
/// assert!(s.contains(b"o:3"));
/// assert_eq!(s.rows(), 3);
/// assert_eq!(s.group(b"eu").avg(), Some(21.0));
/// let top = s.top_groups(AggBy::Sum, 1);
/// assert_eq!(top[0].0, b"us");
/// assert_eq!(s.all_groups().len(), 2);
/// ```
#[derive(Debug, Default)]
pub struct AggSegment {
    groups: HashMap<Vec<u8>, Group>,
    /// row key → (group, value) for O(log) update/remove.
    rows: HashMap<Vec<u8>, (Vec<u8>, IndexValue)>,
    excluded: u64,
    /// Running counters, so `stats()` never walks the maps
    /// (the walk ran on every tiering tick). Each mirrors one walking
    /// term of the byte formula; `recompute_stats` is the reference
    /// the tests hold them to.
    distinct_total: u64,
    gkey_bytes: u64,
    row_key_bytes: u64,
}

impl AggSegment {
    /// Empty segment.
    pub fn new() -> Self {
        Self::default()
    }

    /// (Re-)register one row under what it now contributes.
    // missing_panics_doc: the only panic is the "group of live row" expect —
    // an internal rows↔groups invariant, never reachable from caller input.
    #[allow(clippy::missing_panics_doc)]
    pub fn apply(&mut self, key: &[u8], row: AggRow) {
        let (entry, excluded_row) = match row {
            AggRow::Member { group, value } => (Some((group, value)), false),
            AggRow::Removed => (None, false),
            AggRow::Excluded => (None, true),
        };
        if let Some((group, val)) = &entry
            && self.fast_path_same_group(key, group, val)
        {
            return;
        }
        self.retract_row(key);
        match entry {
            Some((group, val)) => {
                let g = self.groups.entry(group.clone()).or_insert(Group {
                    count: 0,
                    sum: 0.0,
                    values: BTreeMap::new(),
                });
                if g.count == 0 {
                    self.gkey_bytes += group.len() as u64;
                }
                g.count += 1;
                g.sum += val.as_f64();
                let slot = g.values.entry(val.clone()).or_insert(0);
                *slot += 1;
                if *slot == 1 {
                    self.distinct_total += 1;
                }
                self.row_key_bytes += key.len() as u64 + 10;
                self.rows.insert(key.to_vec(), (group, val));
            }
            None if excluded_row => self.excluded += 1,
            None => {}
        }
    }

    /// Fast path: same-group value update (the dominant serving write
    /// shape — measured 16.8% write tax on a Zipf corpus with the
    /// retract+register path; hot groups make every full retract pay
    /// two map round-trips and two clones). `true` = handled.
    fn fast_path_same_group(&mut self, key: &[u8], group: &[u8], val: &IndexValue) -> bool {
        let Some((old_group, old_val)) = self.rows.get_mut(key) else { return false };
        if old_group != group {
            return false;
        }
        if old_val == val {
            return true; // nothing changed
        }
        let g = self.groups.get_mut(group).expect("group of live row");
        g.sum += val.as_f64() - old_val.as_f64();
        match g.values.get_mut(old_val) {
            Some(m) if *m > 1 => *m -= 1,
            _ => {
                g.values.remove(old_val);
                self.distinct_total -= 1;
            }
        }
        let slot = g.values.entry(val.clone()).or_insert(0);
        *slot += 1;
        if *slot == 1 {
            self.distinct_total += 1;
        }
        *old_val = val.clone();
        true
    }

    /// Retract one row's current contribution (no-op for an unknown
    /// key); drops the group when its last row leaves.
    fn retract_row(&mut self, key: &[u8]) {
        if let Some((old_group, old_val)) = self.rows.remove(key) {
            self.row_key_bytes -= key.len() as u64 + 10;
            let empty = {
                let g = self.groups.get_mut(&old_group).expect("group of live row");
                g.count -= 1;
                g.sum -= old_val.as_f64();
                match g.values.get_mut(&old_val) {
                    Some(m) if *m > 1 => *m -= 1,
                    _ => {
                        g.values.remove(&old_val);
                        self.distinct_total -= 1;
                    }
                }
                g.count == 0
            };
            if empty {
                self.groups.remove(&old_group);
                self.gkey_bytes -= old_group.len() as u64;
            }
        }
    }

    /// One group's stats (`count == 0` shape for an unknown group).
    pub fn group(&self, group: &[u8]) -> GroupStats {
        match self.groups.get(group) {
            Some(g) => GroupStats {
                count: g.count,
                sum: g.sum,
                min: g.values.keys().next().cloned(),
                max: g.values.keys().next_back().cloned(),
            },
            None => GroupStats { count: 0, sum: 0.0, min: None, max: None },
        }
    }

    /// Top `limit` groups ranked by `by` (count/sum/max descending,
    /// min ascending), ties broken by group key ascending.
    ///
    /// Bounded selection over BORROWED keys — the first cut cloned
    /// and sorted every group per query (measured as the dominant
    /// per-shard cost at 10k groups); only the winners materialize.
    pub fn top_groups(&self, by: AggBy, limit: usize) -> Vec<(Vec<u8>, GroupStats)> {
        let score_of = |g: &Group| -> f64 {
            match by {
                AggBy::Count => g.count as f64,
                AggBy::Sum => g.sum,
                AggBy::Max => {
                    g.values.keys().next_back().map_or(f64::NEG_INFINITY, IndexValue::as_f64)
                }
                AggBy::Min => g.values.keys().next().map_or(f64::NEG_INFINITY, |v| -v.as_f64()),
            }
        };
        // float_cmp: exact equality is the tiebreak trigger — an epsilon would
        // make the top-K selection non-deterministic for equal scores.
        #[allow(clippy::float_cmp)]
        let better = |a: (f64, &[u8]), b: (f64, &[u8])| a.0 > b.0 || (a.0 == b.0 && a.1 < b.1);
        let mut top: Vec<(f64, &Vec<u8>)> = Vec::with_capacity(limit.min(1024) + 1);
        for (k, g) in &self.groups {
            let cand = (score_of(g), k);
            if top.len() < limit {
                top.push(cand);
                if top.len() == limit {
                    top.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                }
            } else if let Some(last) = top.last()
                && better((cand.0, cand.1), (last.0, last.1))
            {
                let pos = top.partition_point(|e| better((e.0, e.1), (cand.0, cand.1)));
                top.insert(pos, cand);
                top.pop();
            }
        }
        if top.len() < limit {
            top.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        }
        top.into_iter().map(|(_, k)| (k.clone(), self.group(k))).collect()
    }

    /// Every group, UNRANKED — the fan-out chunk shape (ranking
    /// happens once, at the reduce, after cross-shard merge; sorting
    /// per shard would be wasted work).
    pub fn all_groups(&self) -> Vec<(Vec<u8>, GroupStats)> {
        self.groups.keys().map(|k| (k.clone(), self.group(k))).collect()
    }

    /// Membership probe (verify hook).
    pub fn contains(&self, key: &[u8]) -> bool {
        self.rows.contains_key(key)
    }

    /// This segment's live row count — the one field of [`Self::stats`]
    /// that is a `len()`. Its own accessor because `stats` also sums every
    /// group's values, every group key and every row key to estimate bytes,
    /// and a caller asking "how many rows" should not pay for that.
    pub fn rows(&self) -> u64 {
        self.rows.len() as u64
    }

    /// Live counters — O(1): every term is a running
    /// counter maintained at the mutation sites. Byte constants
    /// calibrated against measured RSS growth at 1M rows / 10k Zipf
    /// groups (the first-cut 40/24 constants overestimated 2× —
    /// BTreeMap packs ~11 entries per node and the reverse map's vecs
    /// are small-alloc pooled).
    pub fn stats(&self) -> AggStats {
        AggStats {
            groups: self.groups.len() as u64,
            rows: self.rows.len() as u64,
            excluded: self.excluded,
            approx_bytes: self.gkey_bytes
                + self.groups.len() as u64 * 64
                + self.distinct_total * 18
                + self.row_key_bytes,
        }
    }

    /// The walking reference — recomputes every byte term from the
    /// live maps. Test-only: production reads the running counters.
    #[cfg(test)]
    pub(crate) fn recompute_stats(&self) -> AggStats {
        let distinct: u64 = self.groups.values().map(|g| g.values.len() as u64).sum();
        let gkey: u64 = self.groups.keys().map(|k| k.len() as u64).sum();
        let rowbytes: u64 = self.rows.keys().map(|k| (k.len() + 10) as u64).sum();
        AggStats {
            groups: self.groups.len() as u64,
            rows: self.rows.len() as u64,
            excluded: self.excluded,
            approx_bytes: gkey + self.groups.len() as u64 * 64 + distinct * 18 + rowbytes,
        }
    }
}

#[cfg(test)]
#[path = "agg_tests.rs"]
mod tests;
