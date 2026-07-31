//! The clause-carrying scalar query — `FILTER` / `SORT` / `DISTINCT` /
//! `FACET` / `OFFSET` over one shard's [`Segment`] — and the origin-side
//! merge every embedding runs the same way.
//!
//! The semantics mirror the text surface exactly (kevy-text's
//! `segment_query` / `segment_select`), re-stated here for the scalar
//! kinds:
//!
//! * `FILTER` — non-scoring predicates, ANDed; a candidate without the
//!   stored value FAILS (absent is not a value).
//! * `SORT` — selection by the stored value's order key; a row with no
//!   usable value sorts LAST in both directions; ties break by row key.
//! * `DISTINCT` — at most one row per coerced value identity, collapsed
//!   DURING selection; a row with no value is its own group.
//! * `FACET` — value counts over the WHOLE match set (after `FILTER`,
//!   before any truncation; `DISTINCT` does not reduce them).
//! * `OFFSET` — applied at the origin over the merged page; each shard
//!   returns `limit + offset`.

use std::collections::HashMap;
use std::ops::Bound;

use crate::catalog::ValType;
use crate::value::ValueTest;
use crate::segment::{Cursor, Segment};
use crate::value::{IndexValue, order_key};

/// Everything a scalar query carries beyond its bounds. Field indices
/// are positions into the spec's declared `VALUES` list; the caller
/// resolves names (and errors on unknown ones) before building this.
pub struct ScalarClauses<'a> {
    /// `(stored-value position, typed test)` per `FILTER`, ANDed.
    pub filters: &'a [(usize, ValueTest)],
    /// `SORT`: `(position, desc, declared type)`.
    pub sort: Option<(usize, bool, ValType)>,
    /// `DISTINCT`: `(position, declared type)`.
    pub distinct: Option<(usize, ValType)>,
    /// `FACET`: `(position, declared type)` per requested field.
    pub facets: &'a [(usize, ValType)],
    /// How many hits this shard returns (`limit + offset` — the origin
    /// drains the offset after the merge).
    pub fetch: usize,
}

impl ScalarClauses<'_> {
    /// Whether any clause reshapes selection (vs FILTER, which only
    /// thins the driving order and stays cursor-compatible).
    pub fn selects(&self) -> bool {
        self.sort.is_some() || self.distinct.is_some() || !self.facets.is_empty()
    }
}

/// One selected row: its key and indexed value, plus the sort /
/// distinct keys the origin merge needs (only present when the query
/// carried the clause).
pub struct ScalarHit {
    /// Row key.
    pub key: Vec<u8>,
    /// The indexed (driving) value.
    pub value: IndexValue,
    /// The sort field's order-preserving key (`None` = no usable value).
    pub okey: Option<Vec<u8>>,
    /// The distinct field's coerced identity (`None` = own group).
    pub dkey: Option<Vec<u8>>,
}

/// One facet bucket: the identity a cross-shard merge sums by, a
/// spelling that occurs in the corpus, and the count.
pub type FacetBucket = (Vec<u8>, Vec<u8>, u64);

/// One facet field's in-flight counts: identity → (label, count).
type FacetCounts = HashMap<Vec<u8>, (Vec<u8>, u64)>;

/// One shard's clause-carrying page.
pub struct ClausedPage {
    /// The selected hits (driving order, or sort order under `SORT`).
    pub hits: Vec<ScalarHit>,
    /// Per requested facet field, its buckets over this shard's match
    /// set.
    pub facets: Vec<Vec<FacetBucket>>,
    /// Resume cursor — only ever `Some` on the FILTER-with-CURSOR path
    /// (selection clauses refuse cursors at the surface).
    pub cursor: Option<Cursor>,
}

/// The order a page sorted by a stored value is in: a row WITH a value
/// outranks one without (in both directions — missing is not a value),
/// then by the value's order key, then by row key so ties are stable.
///
/// Public because the origin merge must order the union of the shards'
/// pages exactly as each shard ordered its own. Semantics identical to
/// kevy-text's `sorted_order` (two crates, one contract — pinned by
/// tests on both sides).
pub fn scalar_sorted_order(
    a: (Option<&[u8]>, &[u8]),
    b: (Option<&[u8]>, &[u8]),
    desc: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.0, b.0) {
        (Some(x), Some(y)) => {
            let ord = if desc { y.cmp(x) } else { x.cmp(y) };
            ord.then_with(|| a.1.cmp(b.1))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.1.cmp(b.1),
    }
}

impl Segment {
    /// Whether `key` satisfies every predicate (ANDed). A row with no
    /// value for a filtered field never passes; a segment that stores
    /// no values at all fails every filtered candidate — absent is not
    /// a value, in either shape.
    fn passes(&self, key: &[u8], filters: &[(usize, ValueTest)]) -> bool {
        if filters.is_empty() {
            return true;
        }
        filters
            .iter()
            .all(|(f, t)| self.stored(key, *f).is_some_and(|raw| t.passes(raw)))
    }

    /// A stored value's coerced key for a clause: order key under the
    /// declared type; `None` = no usable value (its own group / sorts
    /// last).
    fn clause_key(&self, key: &[u8], field: usize, ty: ValType) -> Option<Vec<u8>> {
        self.stored(key, field).and_then(|raw| order_key(ty, raw))
    }

    /// The clause-carrying count of `[min, max]`: the full walk with
    /// the FILTER predicates applied, materializing nothing — the
    /// total a claused query would reach, without building pages. The
    /// consumer shape this closes: counting a filtered axis used to
    /// mean fetching every page and taking `len`.
    pub fn count_claused(
        &self,
        min: &IndexValue,
        max: &IndexValue,
        filters: &[(usize, ValueTest)],
    ) -> u64 {
        self.range_iter(min, max, None)
            .filter(|(_, k)| self.passes(k, filters))
            .count() as u64
    }

    /// The clause-carrying scan of `[min, max]`. FILTER-only queries
    /// stream in driving order and stay cursor-paged; any selection
    /// clause walks deeper (the whole range for `SORT` / `FACET`) and
    /// returns no cursor.
    pub fn query_claused(
        &self,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
        c: &ScalarClauses<'_>,
    ) -> ClausedPage {
        let mut facets: Vec<FacetCounts> = vec![HashMap::new(); c.facets.len()];
        let mut hits: Vec<ScalarHit> = Vec::new();
        let mut groups: HashMap<Vec<u8>, usize> = HashMap::new();
        // Selection needs the whole match set when sorting (top-K by the
        // sort key) or faceting (counts before truncation); otherwise
        // the walk stops as soon as the page is full.
        let full_walk = c.sort.is_some() || !c.facets.is_empty();
        for (v, k) in self.range_iter(min, max, cursor) {
            if !self.passes(k, c.filters) {
                continue;
            }
            self.count_facets(k, c, &mut facets);
            if !full_walk && hits.len() == c.fetch {
                break;
            }
            self.select_hit(v, k, c, &mut hits, &mut groups);
        }
        if let Some((_, desc, _)) = c.sort {
            hits.sort_by(|a, b| {
                scalar_sorted_order((a.okey.as_deref(), &a.key), (b.okey.as_deref(), &b.key), desc)
            });
        }
        hits.truncate(c.fetch);
        let cursor = self.filter_cursor(c, &hits);
        ClausedPage { hits, facets: finish_facets(facets), cursor }
    }

    /// The streaming `[min, max]` walk, resuming past `cursor`.
    fn range_iter<'s>(
        &'s self,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
    ) -> impl Iterator<Item = (&'s IndexValue, &'s [u8])> {
        let lower: Bound<(IndexValue, Vec<u8>)> = match cursor {
            Some(c) => Bound::Excluded((c.value.clone(), c.key.clone())),
            None => Bound::Included((min.clone(), Vec::new())),
        };
        let max = max.clone();
        self.tree()
            .range((lower, Bound::Unbounded))
            .take_while(move |(v, _)| *v <= max)
            .map(|(v, k)| (v, k.as_slice()))
    }

    /// Credit one passing candidate to every facet bucket it has a
    /// value in. Buckets key by the coerced identity; the label is a
    /// spelling that occurs in the corpus. Rows without a value (or
    /// with one that does not coerce) are in no bucket.
    fn count_facets(
        &self,
        key: &[u8],
        c: &ScalarClauses<'_>,
        facets: &mut [FacetCounts],
    ) {
        for ((f, ty), counts) in c.facets.iter().zip(facets.iter_mut()) {
            let Some(raw) = self.stored(key, *f) else { continue };
            let Some(id) = order_key(*ty, raw) else { continue };
            let e = counts.entry(id).or_insert_with(|| (raw.to_vec(), 0));
            e.1 += 1;
        }
    }

    /// Push one passing candidate onto the page, collapsing under
    /// `DISTINCT` during selection: in driving order the first
    /// occurrence of a value is its best; under `SORT` the better group
    /// representative by the page's own order replaces the held one.
    /// Rows with no value are their own group and never collapse.
    fn select_hit(
        &self,
        v: &IndexValue,
        k: &[u8],
        c: &ScalarClauses<'_>,
        hits: &mut Vec<ScalarHit>,
        groups: &mut HashMap<Vec<u8>, usize>,
    ) {
        let okey = c.sort.and_then(|(f, _, ty)| self.clause_key(k, f, ty));
        let dkey = c.distinct.and_then(|(f, ty)| self.clause_key(k, f, ty));
        if let Some(id) = &dkey {
            match groups.entry(id.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    let Some((_, desc, _)) = c.sort else { return };
                    let prev = &mut hits[*e.get()];
                    if scalar_sorted_order(
                        (okey.as_deref(), k),
                        (prev.okey.as_deref(), &prev.key),
                        desc,
                    ) == std::cmp::Ordering::Less
                    {
                        *prev = ScalarHit { key: k.to_vec(), value: v.clone(), okey, dkey };
                    }
                    return;
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(hits.len());
                }
            }
        }
        hits.push(ScalarHit { key: k.to_vec(), value: v.clone(), okey, dkey });
    }

    /// The resume cursor for the FILTER-with-CURSOR path: the last
    /// served `(value, key)`, exactly as the plain range emits it.
    /// Selection clauses page nothing, so they carry none.
    fn filter_cursor(&self, c: &ScalarClauses<'_>, hits: &[ScalarHit]) -> Option<Cursor> {
        if c.selects() || hits.len() < c.fetch {
            return None;
        }
        hits.last().map(|h| Cursor { value: h.value.clone(), key: h.key.clone() })
    }
}

/// Order the finished buckets for reporting: most frequent first, label
/// breaking ties so two shards counting the same corpus report the same
/// order.
fn finish_facets(facets: Vec<FacetCounts>) -> Vec<Vec<FacetBucket>> {
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

/// The origin-side merge: order the union of the shards' pages exactly
/// as each shard ordered its own (`sort_desc` = the SORT direction, or
/// `None` for the driving `(value, key)` order), re-collapse under
/// `DISTINCT`, drain the offset, cut to the limit. Correct for any
/// per-shard-consistent total order — which is exactly what each shard
/// guarantees. `T` is whatever rides with a hit (the server's hydration
/// block; `()` embedded).
pub fn merge_claused<T>(
    mut all: Vec<(ScalarHit, T)>,
    sort_desc: Option<bool>,
    grouped: bool,
    offset: usize,
    limit: usize,
) -> Vec<(ScalarHit, T)> {
    match sort_desc {
        Some(desc) => all.sort_by(|(a, _), (b, _)| {
            scalar_sorted_order((a.okey.as_deref(), &a.key), (b.okey.as_deref(), &b.key), desc)
        }),
        None => all.sort_by(|(a, _), (b, _)| (&a.value, &a.key).cmp(&(&b.value, &b.key))),
    }
    if grouped {
        // First occurrence in the final order is the group's best; a row
        // with no value is its own group and always survives.
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        all.retain(|(h, _)| match &h.dkey {
            Some(k) => seen.insert(k.clone()),
            None => true,
        });
    }
    if offset > 0 {
        all.drain(..offset.min(all.len()));
    }
    all.truncate(limit);
    all
}

/// Fold one shard's facet buckets into the origin's running totals —
/// summed by identity, not label (two shards can spell `1` and `1.0`);
/// the label kept is the first seen, so it always occurs in the corpus.
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
pub fn sort_facets(facets: &mut [Vec<FacetBucket>]) {
    for f in facets.iter_mut() {
        f.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    }
}

#[cfg(test)]
#[path = "segment_claused_tests.rs"]
mod tests;
