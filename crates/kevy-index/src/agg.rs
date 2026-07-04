//! [`AggSegment`] — one shard's slice of one aggregate index (RFC
//! v3.1, KIND agg): per-group count / sum / min / max maintained
//! synchronously with writes. min/max stay EXACT under deletion via a
//! per-group value multiset (BTreeMap value → multiplicity); a
//! row → (group, value) reverse map supports O(log) updates — the
//! same derived-by-construction discipline as [`crate::Segment`].

use std::collections::{BTreeMap, HashMap};

use crate::IndexValue;

/// One group's live statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupStats {
    /// Rows in the group.
    pub count: u64,
    /// Sum of the aggregated field (f64 accumulation — the i64
    /// overflow guard; precision bounds documented).
    pub sum: f64,
    /// Exact minimum (None only when count == 0).
    pub min: Option<IndexValue>,
    /// Exact maximum.
    pub max: Option<IndexValue>,
}

impl GroupStats {
    /// Derived average.
    pub fn avg(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }
}

struct Group {
    count: u64,
    sum: f64,
    /// value → multiplicity; min/max = first/last key.
    values: BTreeMap<IndexValue, u32>,
}

/// Ranking metric for [`AggSegment::top_groups`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AggBy {
    /// By row count (default).
    #[default]
    Count,
    /// By sum.
    Sum,
    /// By minimum (ascending — smallest mins first).
    Min,
    /// By maximum (descending — largest maxes first).
    Max,
}

impl AggBy {
    /// Wire tag.
    pub fn parse(raw: &[u8]) -> Option<AggBy> {
        if raw.eq_ignore_ascii_case(b"count") {
            Some(AggBy::Count)
        } else if raw.eq_ignore_ascii_case(b"sum") {
            Some(AggBy::Sum)
        } else if raw.eq_ignore_ascii_case(b"min") {
            Some(AggBy::Min)
        } else if raw.eq_ignore_ascii_case(b"max") {
            Some(AggBy::Max)
        } else {
            None
        }
    }
}

/// Sizing counters (memory formula / IDX.LIST).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AggStats {
    /// Live groups.
    pub groups: u64,
    /// Rows participating.
    pub rows: u64,
    /// Rows excluded (coerce failure / missing group field).
    pub excluded: u64,
    /// Approximate heap bytes (RFC D4 formula's measured side).
    pub approx_bytes: u64,
}

/// One shard's aggregate segment.
#[derive(Default)]
pub struct AggSegment {
    groups: HashMap<Vec<u8>, Group>,
    /// row key → (group, value) for O(log) update/remove.
    rows: HashMap<Vec<u8>, (Vec<u8>, IndexValue)>,
    excluded: u64,
}

impl AggSegment {
    /// Empty segment.
    pub fn new() -> Self {
        Self::default()
    }

    /// (Re-)register one row: `Some((group, value))` = row
    /// participates; `None` = removed or excluded. `excluded_row`
    /// marks the None case as a coercion/missing-field exclusion
    /// (counted) rather than a plain delete.
    pub fn apply(&mut self, key: &[u8], entry: Option<(Vec<u8>, IndexValue)>, excluded_row: bool) {
        // Fast path: same-group value update (the dominant serving
        // write shape — measured 16.8% write tax on a Zipf corpus
        // with the retract+register path; hot groups make every
        // full retract pay two map round-trips and two clones).
        if let Some((group, val)) = &entry
            && let Some((old_group, old_val)) = self.rows.get_mut(key)
            && old_group == group
        {
            if old_val == val {
                return; // nothing changed
            }
            let g = self.groups.get_mut(group).expect("group of live row");
            g.sum += val.as_f64() - old_val.as_f64();
            match g.values.get_mut(old_val) {
                Some(m) if *m > 1 => *m -= 1,
                _ => {
                    g.values.remove(old_val);
                }
            }
            *g.values.entry(val.clone()).or_insert(0) += 1;
            *old_val = val.clone();
            return;
        }
        if let Some((old_group, old_val)) = self.rows.remove(key) {
            let empty = {
                let g = self.groups.get_mut(&old_group).expect("group of live row");
                g.count -= 1;
                g.sum -= old_val.as_f64();
                match g.values.get_mut(&old_val) {
                    Some(m) if *m > 1 => *m -= 1,
                    _ => {
                        g.values.remove(&old_val);
                    }
                }
                g.count == 0
            };
            if empty {
                self.groups.remove(&old_group);
            }
        }
        match entry {
            Some((group, val)) => {
                let g = self.groups.entry(group.clone()).or_insert(Group {
                    count: 0,
                    sum: 0.0,
                    values: BTreeMap::new(),
                });
                g.count += 1;
                g.sum += val.as_f64();
                *g.values.entry(val.clone()).or_insert(0) += 1;
                self.rows.insert(key.to_vec(), (group, val));
            }
            None if excluded_row => self.excluded += 1,
            None => {}
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
                AggBy::Max => g.values.keys().next_back().map_or(f64::NEG_INFINITY, IndexValue::as_f64),
                AggBy::Min => g.values.keys().next().map_or(f64::NEG_INFINITY, |v| -v.as_f64()),
            }
        };
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

    /// Live counters. Byte constants calibrated against measured RSS
    /// growth at 1M rows / 10k Zipf groups (the first-cut 40/24
    /// constants overestimated 2× — BTreeMap packs ~11 entries per
    /// node and the reverse map's vecs are small-alloc pooled).
    pub fn stats(&self) -> AggStats {
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

/// Shared ranking order (per-shard AND at the reduce after merging
/// shard partials — one definition, no drift).
pub fn sort_groups(all: &mut [(Vec<u8>, GroupStats)], by: AggBy) {
    match by {
        AggBy::Count => all.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0))),
        AggBy::Sum => all.sort_by(|a, b| b.1.sum.total_cmp(&a.1.sum).then_with(|| a.0.cmp(&b.0))),
        AggBy::Min => all.sort_by(|a, b| {
            match (&a.1.min, &b.1.min) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| a.0.cmp(&b.0))
        }),
        AggBy::Max => all.sort_by(|a, b| {
            match (&b.1.max, &a.1.max) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| a.0.cmp(&b.0))
        }),
    }
}

/// Merge shard partials for one group (reduce side): counts/sums add,
/// min/max take extremes.
pub fn merge_group(into: &mut GroupStats, part: &GroupStats) {
    into.count += part.count;
    into.sum += part.sum;
    into.min = match (into.min.take(), part.min.clone()) {
        (Some(a), Some(b)) => Some(if b < a { b } else { a }),
        (a, b) => a.or(b),
    };
    into.max = match (into.max.take(), part.max.clone()) {
        (Some(a), Some(b)) => Some(if b > a { b } else { a }),
        (a, b) => a.or(b),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg() -> AggSegment {
        let mut s = AggSegment::new();
        // orders: group = status, value = amount
        for (k, g, v) in [
            ("o1", "paid", 100),
            ("o2", "paid", 250),
            ("o3", "open", 40),
            ("o4", "paid", 100),
            ("o5", "open", 999),
        ] {
            s.apply(k.as_bytes(), Some((g.as_bytes().to_vec(), IndexValue::I64(v))), false);
        }
        s
    }

    #[test]
    fn group_stats_exact() {
        let s = seg();
        let g = s.group(b"paid");
        assert_eq!((g.count, g.sum), (3, 450.0));
        assert_eq!(g.min, Some(IndexValue::I64(100)));
        assert_eq!(g.max, Some(IndexValue::I64(250)));
        assert_eq!(g.avg(), Some(150.0));
        let none = s.group(b"nope");
        assert_eq!(none.count, 0);
        assert!(none.min.is_none() && none.avg().is_none());
    }

    #[test]
    fn min_max_exact_under_delete_and_update() {
        let mut s = seg();
        // delete the paid max (o2=250): max must fall back to 100
        s.apply(b"o2", None, false);
        let g = s.group(b"paid");
        assert_eq!((g.count, g.max.clone()), (2, Some(IndexValue::I64(100))));
        // duplicate values: removing ONE 100 keeps the other
        s.apply(b"o1", None, false);
        let g = s.group(b"paid");
        assert_eq!((g.count, g.min.clone()), (1, Some(IndexValue::I64(100))));
        // update moves a row across groups
        s.apply(b"o3", Some((b"paid".to_vec(), IndexValue::I64(40))), false);
        assert_eq!(s.group(b"paid").count, 2);
        assert_eq!(s.group(b"open").count, 1);
        assert_eq!(s.group(b"paid").min, Some(IndexValue::I64(40)));
        // last row leaving a group drops the group
        s.apply(b"o5", None, false);
        assert_eq!(s.group(b"open").count, 0);
        assert_eq!(s.stats().groups, 1);
    }

    #[test]
    fn top_groups_all_metrics() {
        let s = seg();
        let top = s.top_groups(AggBy::Count, 10);
        assert_eq!(top[0].0, b"paid".to_vec());
        let top = s.top_groups(AggBy::Sum, 10);
        assert_eq!(top[0].0, b"open".to_vec(), "open sum 1039 > paid 450");
        let top = s.top_groups(AggBy::Min, 10);
        assert_eq!(top[0].0, b"open".to_vec(), "min ascending: 40 first");
        let top = s.top_groups(AggBy::Max, 1);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].0, b"open".to_vec(), "max 999");
    }

    #[test]
    fn excluded_counted_and_merge() {
        let mut s = seg();
        s.apply(b"bad1", None, true);
        s.apply(b"bad2", None, true);
        assert_eq!(s.stats().excluded, 2);
        assert!(s.contains(b"o1") && !s.contains(b"bad1"));
        // cross-shard merge semantics
        let mut a = s.group(b"paid");
        let b = seg().group(b"paid");
        merge_group(&mut a, &b);
        assert_eq!((a.count, a.sum), (6, 900.0));
        assert_eq!(a.min, Some(IndexValue::I64(100)));
        assert_eq!(a.max, Some(IndexValue::I64(250)));
        // merge with an empty partial keeps extremes
        let mut e = GroupStats { count: 0, sum: 0.0, min: None, max: None };
        merge_group(&mut e, &a);
        assert_eq!(e.max, Some(IndexValue::I64(250)));
    }

    #[test]
    fn stats_bytes_nonzero() {
        let s = seg();
        let st = s.stats();
        assert_eq!((st.groups, st.rows), (2, 5));
        assert!(st.approx_bytes > 0);
    }
}
