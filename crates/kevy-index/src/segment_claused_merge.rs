//! The clause engine's halves that run away from a live [`Segment`]:
//! the walk over decoded cold entries, and the origin-side merge of
//! every shard's page and facet buckets.
//!
//! [`Segment`]: crate::Segment

use std::collections::HashMap;

use crate::catalog::ValType;
use crate::segment_claused::{FacetBucket, FacetCounts, ScalarClauses, ScalarHit};
use crate::value::{IndexValue, ValueTest, order_key};
use kevy_text::{SortOrder, sorted_order};

/// One decoded cold entry: the driving value, the row key, and the
/// stored values that rode in its payload.
///
/// ```
/// use kevy_index::{ColdEntryRow, IndexValue, ValType, decode_seg_key, decode_seg_values,
///     encode_seg_values, seg_key};
/// let key = seg_key(&IndexValue::I64(5), b"row");
/// let payload = encode_seg_values(&[Some(b"v")]);
/// let (value, row) = decode_seg_key(ValType::I64, &key).expect("well-formed");
/// let entry: ColdEntryRow = (value, row, decode_seg_values(&payload).expect("well-formed"));
/// assert_eq!(entry.2, [Some(b"v".to_vec())]);
/// ```
pub type ColdEntryRow = (IndexValue, Vec<u8>, Vec<Option<Vec<u8>>>);

/// The clause-carrying walk over a DECODED stream — the cold twin of
/// [`crate::Segment::query_claused`], one clause engine for entries whose
/// values ride beside them (a cold segment's payload) instead of in
/// the hot `RowValues` map. Same loop, same order: FILTER, facet
/// counts, the early page break, DISTINCT collapse during selection,
/// the SORT re-order, the fetch truncation. The caller owns I/O,
/// decoding, tombstones and cursors — this walk never sees them.
///
/// ```
/// use kevy_index::{IndexValue, ScalarClauses, SortOrder, ValType, claused_over};
/// let rows = vec![
///     (IndexValue::I64(1), b"a".to_vec(), vec![Some(b"30".to_vec())]),
///     (IndexValue::I64(2), b"b".to_vec(), vec![Some(b"10".to_vec())]),
///     (IndexValue::I64(3), b"c".to_vec(), vec![None]),
/// ];
/// let c = ScalarClauses::new(10).with_sort(0, SortOrder::Asc, ValType::I64);
/// let (hits, _) = claused_over(rows.into_iter(), &c);
/// let keys: Vec<_> = hits.iter().map(|h| h.key.as_slice()).collect();
/// assert_eq!(keys, [b"b", b"a", b"c"], "no value sorts last");
/// ```
pub fn claused_over(
    items: impl Iterator<Item = ColdEntryRow>,
    c: &ScalarClauses<'_>,
) -> (Vec<ScalarHit>, Vec<Vec<FacetBucket>>) {
    let mut facets: Vec<FacetCounts> = vec![HashMap::new(); c.facets.len()];
    let mut hits: Vec<ScalarHit> = Vec::new();
    let mut groups: HashMap<Vec<u8>, usize> = HashMap::new();
    let full_walk = c.sort.is_some() || !c.facets.is_empty();
    for (v, k, vals) in items {
        if !values_pass(&vals, c.filters) {
            continue;
        }
        for ((f, ty), counts) in c.facets.iter().zip(facets.iter_mut()) {
            let Some(raw) = vals.get(*f).and_then(Option::as_deref) else { continue };
            let Some(id) = order_key(*ty, raw) else { continue };
            counts.entry(id).or_insert_with(|| (raw.to_vec(), 0)).1 += 1;
        }
        if !full_walk && hits.len() == c.fetch {
            break;
        }
        select_values_hit(v, k, &vals, c, &mut hits, &mut groups);
    }
    if let Some((_, order, _)) = c.sort {
        hits.sort_by(|a, b| {
            sorted_order((a.okey.as_deref(), &a.key), (b.okey.as_deref(), &b.key), order)
        });
    }
    hits.truncate(c.fetch);
    (hits, finish_facets(facets))
}

/// Whether decoded values satisfy every predicate — the rule a
/// segment's own `FILTER` applies: absent is not a value.
///
/// ```
/// use kevy_index::{ValType, ValueTest, values_pass};
/// let f = [(0, ValueTest::range(ValType::I64, b"18", b"64").expect("an i64 range"))];
/// assert!(values_pass(&[Some(b"30".to_vec())], &f));
/// assert!(!values_pass(&[Some(b"70".to_vec())], &f));
/// assert!(!values_pass(&[None], &f), "absent is not a value");
/// assert!(values_pass(&[None], &[]));
/// ```
pub fn values_pass(values: &[Option<Vec<u8>>], filters: &[(usize, ValueTest)]) -> bool {
    filters
        .iter()
        .all(|(f, t)| values.get(*f).and_then(Option::as_deref).is_some_and(|raw| t.passes(raw)))
}

/// A decoded value's coerced clause key — `Segment::clause_key`'s
/// rule over a payload row.
fn values_clause_key(values: &[Option<Vec<u8>>], field: usize, ty: ValType) -> Option<Vec<u8>> {
    values.get(field).and_then(Option::as_deref).and_then(|raw| order_key(ty, raw))
}

/// `Segment::select_hit` over a decoded entry — one selection rule,
/// re-stated for values that arrived beside the key.
fn select_values_hit(
    v: IndexValue,
    k: Vec<u8>,
    vals: &[Option<Vec<u8>>],
    c: &ScalarClauses<'_>,
    hits: &mut Vec<ScalarHit>,
    groups: &mut HashMap<Vec<u8>, usize>,
) {
    let okey = c.sort.and_then(|(f, _, ty)| values_clause_key(vals, f, ty));
    let dkey = c.distinct.and_then(|(f, ty)| values_clause_key(vals, f, ty));
    if let Some(id) = &dkey {
        match groups.entry(id.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => {
                let Some((_, order, _)) = c.sort else { return };
                let prev = &mut hits[*e.get()];
                if sorted_order((okey.as_deref(), &k), (prev.okey.as_deref(), &prev.key), order)
                    == std::cmp::Ordering::Less
                {
                    *prev = ScalarHit { key: k, value: v, okey, dkey };
                }
                return;
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(hits.len());
            }
        }
    }
    hits.push(ScalarHit { key: k, value: v, okey, dkey });
}

/// The origin-side merge: order the union of the shards' pages exactly
/// as each shard ordered its own (`sort` = the SORT direction, or `None`
/// for the driving `(value, key)` order), re-collapse hits that carry a
/// `DISTINCT` identity, drain the offset, cut to the limit. Correct for
/// any per-shard-consistent total order — which is exactly what each
/// shard guarantees. `T` is whatever rides with a hit (the server's
/// hydration block; `()` embedded).
///
/// Only a query with `DISTINCT` gives its hits an identity
/// ([`ScalarHit::dkey`]), so the collapse needs no switch of its own.
///
/// ```
/// use kevy_index::{IndexValue, ScalarHit, merge_claused};
/// let hit = |k: &[u8], v| (ScalarHit::new(k.to_vec(), IndexValue::I64(v)), ());
/// let shard_a = [hit(b"a", 1), hit(b"c", 3)];
/// let shard_b = [hit(b"b", 2), hit(b"d", 4)];
/// let all = shard_a.into_iter().chain(shard_b).collect();
/// let page = merge_claused(all, None, 1, 2);
/// let keys: Vec<_> = page.iter().map(|(h, _)| h.key.as_slice()).collect();
/// assert_eq!(keys, [b"b", b"c"], "offset 1, limit 2 over the merged order");
/// ```
pub fn merge_claused<T>(
    mut all: Vec<(ScalarHit, T)>,
    sort: Option<SortOrder>,
    offset: usize,
    limit: usize,
) -> Vec<(ScalarHit, T)> {
    match sort {
        Some(order) => all.sort_by(|(a, _), (b, _)| {
            sorted_order((a.okey.as_deref(), &a.key), (b.okey.as_deref(), &b.key), order)
        }),
        None => all.sort_by(|(a, _), (b, _)| (&a.value, &a.key).cmp(&(&b.value, &b.key))),
    }
    // First occurrence in the final order is the group's best; a row
    // with no identity is its own group and always survives.
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    all.retain(|(h, _)| match &h.dkey {
        Some(k) => seen.insert(k.clone()),
        None => true,
    });
    if offset > 0 {
        all.drain(..offset.min(all.len()));
    }
    all.truncate(limit);
    all
}

/// Fold one shard's facet buckets into the origin's running totals —
/// summed by identity, not label (two shards can spell `1` and `1.0`);
/// the label kept is the first seen, so it always occurs in the corpus.
///
/// ```
/// use kevy_index::{FacetBucket, fold_facets};
/// let mut total: Vec<Vec<FacetBucket>> = vec![vec![(b"k".to_vec(), b"kyoto".to_vec(), 2)]];
/// fold_facets(&mut total, vec![vec![(b"o".to_vec(), b"osaka".to_vec(), 5)]]);
/// assert_eq!(total[0].len(), 2);
/// ```
pub fn fold_facets(into: &mut [Vec<FacetBucket>], from: Vec<Vec<FacetBucket>>) {
    for (acc, part) in into.iter_mut().zip(from) {
        for (id, label, n) in part {
            match acc.iter_mut().find(|(k, _, _)| *k == id) {
                Some(e) => e.2 += n,
                None => acc.push((id, label, n)),
            }
        }
    }
}

/// Order folded buckets for the reply: most frequent first, label
/// breaking ties (the same rule each shard reported with).
///
/// ```
/// use kevy_index::sort_facets;
/// let mut f = vec![vec![
///     (b"1".to_vec(), b"b".to_vec(), 1),
///     (b"2".to_vec(), b"c".to_vec(), 4),
///     (b"3".to_vec(), b"a".to_vec(), 1),
/// ]];
/// sort_facets(&mut f);
/// let labels: Vec<_> = f[0].iter().map(|b| b.1.as_slice()).collect();
/// assert_eq!(labels, [b"c", b"a", b"b"], "most frequent first, then by label");
/// ```
pub fn sort_facets(facets: &mut [Vec<FacetBucket>]) {
    for f in facets.iter_mut() {
        f.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    }
}

/// Order the finished buckets for reporting: most frequent first, label
/// breaking ties so two shards counting the same corpus report the same
/// order.
pub(crate) fn finish_facets(facets: Vec<FacetCounts>) -> Vec<Vec<FacetBucket>> {
    facets
        .into_iter()
        .map(|counts| {
            let mut out: Vec<FacetBucket> =
                counts.into_iter().map(|(id, (label, n))| (id, label, n)).collect();
            out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
            out
        })
        .collect()
}
