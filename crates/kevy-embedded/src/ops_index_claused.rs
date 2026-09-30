//! The clause-carrying scalar query for the embedded API — `FILTER` /
//! `SORT` / `DISTINCT` / `FACET` / `OFFSET` on Range/Unique indexes
//! (the capacity arc's G1), plus [`ValueFilter`], the predicate shape
//! this surface shares with the text MATCH clauses. A `#[path]` child
//! of `ops_index.rs`, feature-independent of `text`.

use kevy_index::{
    Cursor, FacetBucket, IndexSpec, IndexValue, ScalarClauses, ScalarHit, ValType, ValueTest,
    fold_facets, merge_claused, sort_facets,
};

pub(crate) use super::opts::{ScalarQueryOpts, ValueFilter};
use super::sync_segs;
use crate::store::{Store, lock_write};
use crate::{KevyError, KevyResult};

/// One `FILTER` predicate resolved against the spec: the stored-value
/// position it reads, and the test built with that field's DECLARED type.
pub(crate) fn value_test(spec: &IndexSpec, f: &ValueFilter<'_>) -> KevyResult<(usize, ValueTest)> {
    let stored: Vec<&[u8]> = spec.values().iter().map(|v| v.name.as_slice()).collect();
    let pos = spec
        .values()
        .iter()
        .position(|v| v.name == f.field())
        .ok_or_else(|| unknown_field("FILTER", f.field(), "store", &stored))?;
    let ty = spec.values()[pos].ty;
    // FILTER bounds speak the `@` time expressions on i64 fields —
    // one grammar across both faces (and, through this shared
    // resolver, the embedded Rust API too).
    let now = (kevy_store::now_unix_ms() / 1000) as i64;
    let (test, raw) = match f {
        ValueFilter::Range { min, max, .. } => (ValueTest::range_at(ty, min, max, now), *min),
        ValueFilter::Eq { value, .. } => (ValueTest::eq_at(ty, value, now), *value),
    };
    let test = test.ok_or_else(|| {
        KevyError::InvalidInput(format!(
            "FILTER bound '{}' is not a valid {}, which is how this index declares '{}'",
            String::from_utf8_lossy(raw),
            ty.tag(),
            String::from_utf8_lossy(f.field()),
        ))
    })?;
    Ok((pos, test))
}

/// The first clause field the spec does not store — the advise-log
/// derivation for a refused [`resolve`]. Checked in the clause order
/// [`resolve`] resolves in, so it names the same field the error does.
fn unstored_field(spec: &IndexSpec, opts: &ScalarQueryOpts<'_>) -> Option<Vec<u8>> {
    let stored = |f: &[u8]| spec.values().iter().any(|v| v.name == f);
    for f in opts.filters {
        if !stored(f.field()) {
            return Some(f.field().to_vec());
        }
    }
    if let Some((f, _)) = opts.sort
        && !stored(f)
    {
        return Some(f.to_vec());
    }
    if let Some(f) = opts.distinct
        && !stored(f)
    {
        return Some(f.to_vec());
    }
    opts.facets.iter().find(|f| !stored(f)).cloned()
}

/// A clause naming a field the index does not offer, saying what it does.
pub(crate) fn unknown_field(clause: &str, bad: &[u8], verb: &str, offered: &[&[u8]]) -> KevyError {
    let names: Vec<String> =
        offered.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect();
    KevyError::InvalidInput(format!(
        "{clause} names field '{}', which this index does not {verb} — it {}: {}",
        String::from_utf8_lossy(bad),
        // Third person singular: a sibilant takes -es ("indexes"), anything
        // else takes -s ("stores"). `{verb}es` for both printed "storees".
        if verb.ends_with(['s', 'x', 'z']) { format!("{verb}es") } else { format!("{verb}s") },
        names.join(", ")
    ))
}

/// A clause-carrying query's answer: the page, per requested `FACET`
/// field its `(value, count)` buckets, and — on the FILTER-with-cursor
/// path — the cursor to resume from.
///
/// ```
/// use kevy_embedded::{Config, IndexValue, ScalarQueryOpts, Store};
///
/// let s = Store::open(Config::default())?;
/// s.idx_create(b"by_age", b"u:", b"age", kevy_embedded::IndexValType::I64, kevy_embedded::IndexKind::Range)?;
/// s.hset(b"u:1", &[(b"age", b"30")])?;
/// let page = s.idx_query_claused(b"by_age", &IndexValue::I64(0), &IndexValue::I64(99), None, 10, ScalarQueryOpts::default())?;
/// assert_eq!(page.rows.len(), 1);
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ScalarPage {
    /// The selected rows, in the page's order.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # for (k, age, status) in [(&b"u:1"[..], &b"30"[..], &b"paid"[..]), (b"u:2", b"40", b"paid"), (b"u:3", b"50", b"due")] {
    /// #     s.hset(k, &[(b"age", age), (b"status", status)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// // u:1..u:3 aged 30, 40, 50; two paid, one due
    /// let paid = [ValueFilter::Eq { field: b"status", value: b"paid" }];
    /// let opts = ScalarQueryOpts::default().with_filters(&paid);
    /// let page = s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?;
    /// assert_eq!(page.rows, [(b"u:1".to_vec(), IndexValue::I64(30)), (b"u:2".to_vec(), IndexValue::I64(40))]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub rows: Vec<(Vec<u8>, IndexValue)>,
    /// One entry per requested facet field, most frequent first.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # for (k, age, status) in [(&b"u:1"[..], &b"30"[..], &b"paid"[..]), (b"u:2", b"40", b"paid"), (b"u:3", b"50", b"due")] {
    /// #     s.hset(k, &[(b"age", age), (b"status", status)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// let fields = [b"status".to_vec()];
    /// let opts = ScalarQueryOpts::default().with_facets(&fields);
    /// let page = s.idx_query_claused(b"by_age", &lo, &hi, None, 1, opts)?;
    /// // counted over every match, not just the one-row page
    /// assert_eq!(page.facets, [vec![(b"paid".to_vec(), 2), (b"due".to_vec(), 1)]]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub facets: Vec<Vec<(Vec<u8>, u64)>>,
    /// Resume cursor (`None` under any selection clause, which refuses
    /// cursors at the wire and pages nothing here either).
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # for (k, age, status) in [(&b"u:1"[..], &b"30"[..], &b"paid"[..]), (b"u:2", b"40", b"paid"), (b"u:3", b"50", b"due")] {
    /// #     s.hset(k, &[(b"age", age), (b"status", status)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// let plain = ScalarQueryOpts::default();
    /// let first = s.idx_query_claused(b"by_age", &lo, &hi, None, 2, plain)?;
    /// let rest = s.idx_query_claused(b"by_age", &lo, &hi, first.cursor.as_ref(), 2, plain)?;
    /// assert_eq!(rest.rows[0].0, b"u:3");
    /// let sorted = plain.with_sort(b"status", SortOrder::Asc);
    /// assert!(s.idx_query_claused(b"by_age", &lo, &hi, None, 2, sorted)?.cursor.is_none());
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub cursor: Option<Cursor>,
}

/// The unmerged union of the shards' pages plus the folded facet
/// partials (the payload slot is `()` — the wire's hydration rides only
/// the server's chunks).
type GatheredPages = (Vec<(ScalarHit, ())>, Vec<Vec<FacetBucket>>);

/// What the spec-dependent clauses resolve to.
struct Resolved {
    filters: Vec<(usize, ValueTest)>,
    sort: Option<(usize, kevy_index::SortOrder, ValType)>,
    distinct: Option<(usize, ValType)>,
    facets: Vec<(usize, ValType)>,
}

/// A clause's named stored-value field position + declared type.
fn value_field(spec: &IndexSpec, clause: &str, field: &[u8]) -> KevyResult<(usize, ValType)> {
    let stored: Vec<&[u8]> = spec.values().iter().map(|v| v.name.as_slice()).collect();
    let pos = spec
        .values()
        .iter()
        .position(|v| v.name == field)
        .ok_or_else(|| unknown_field(clause, field, "store", &stored))?;
    Ok((pos, spec.values()[pos].ty))
}

fn resolve(spec: &IndexSpec, opts: &ScalarQueryOpts<'_>) -> KevyResult<Resolved> {
    let filters =
        opts.filters.iter().map(|f| value_test(spec, f)).collect::<KevyResult<Vec<_>>>()?;
    let sort = match opts.sort {
        Some((field, order)) => {
            let (pos, ty) = value_field(spec, "SORT", field)?;
            Some((pos, order, ty))
        }
        None => None,
    };
    let distinct = match opts.distinct {
        Some(field) => Some(value_field(spec, "DISTINCT", field)?),
        None => None,
    };
    let facets = opts
        .facets
        .iter()
        .map(|f| value_field(spec, "FACET", f))
        .collect::<KevyResult<Vec<_>>>()?;
    Ok(Resolved { filters, sort, distinct, facets })
}

impl Store {
    /// [`Store::idx_create`] with declared stored `VALUES` columns —
    /// the scalar kinds' G1 capability (the catalog refuses the
    /// declaration on kinds that carry no stored-value column).
    pub fn idx_create_with_values(
        &self,
        name: &[u8],
        prefix: &[u8],
        field: &[u8],
        ty: ValType,
        kind: kevy_index::IndexKind,
        values: &[(&[u8], ValType)],
    ) -> KevyResult<()> {
        if prefix.is_empty() {
            return Err(KevyError::InvalidInput("empty prefix".into()));
        }
        let values =
            values.iter().map(|(n, t)| kevy_index::ValueSpec::new(*n).with_type(*t)).collect();
        let spec = IndexSpec::builder(name, prefix, kind, ty).with_field(field).with_values(values);
        let spec = crate::ops_index::built(spec)?;
        self.catalog_change(|| self.register_spec(spec))
    }

    /// [`Store::idx_query`] with the stored-value clauses — the scalar
    /// twin of [`Store::idx_match_faceted`]'s clause surface. Semantics
    /// are the wire's: each shard selects with the clauses applied, the
    /// union merges in the page's own order, `DISTINCT` re-collapses,
    /// `OFFSET` drains after the merge, facet counts sum by coerced
    /// identity. Only `FILTER` (with no other clause) is cursor-paged.
    pub fn idx_query_claused(
        &self,
        name: &[u8],
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
        limit: usize,
        opts: ScalarQueryOpts<'_>,
    ) -> KevyResult<ScalarPage> {
        let limit = limit.clamp(1, 100_000);
        let offset = opts.offset.min(10_000);
        let spec = self.claused_spec(name)?;
        let r = self.claused_resolve(name, &spec, &opts)?;
        let mut clauses = ScalarClauses::new(limit + offset).with_filters(&r.filters);
        clauses = clauses.with_facets(&r.facets);
        (clauses.sort, clauses.distinct) = (r.sort, r.distinct);
        let (all, mut facets) = self.gather_claused(name, min, max, cursor, &clauses)?;
        let all = merge_claused(all, r.sort.map(|(_, order, _)| order), offset, limit);
        sort_facets(&mut facets);
        let next = (!opts.selects() && all.len() == limit)
            .then(|| all.last().map(|(h, ())| Cursor::new(h.value.clone(), h.key.clone())))
            .flatten();
        self.observe_hit(name);
        Ok(ScalarPage {
            rows: all.into_iter().map(|(h, ())| (h.key, h.value)).collect(),
            facets: facets
                .into_iter()
                .map(|f| f.into_iter().map(|(_, label, n)| (label, n)).collect())
                .collect(),
            cursor: next,
        })
    }

    /// [`Store::idx_count`] with `FILTER` applied — the total a
    /// claused query's pages would reach, materializing nothing. The
    /// consumer shape this closes: counting a filtered axis used to
    /// mean fetching every page and taking its length.
    pub fn idx_count_claused(
        &self,
        name: &[u8],
        min: &IndexValue,
        max: &IndexValue,
        filters: &[ValueFilter<'_>],
    ) -> KevyResult<u64> {
        let spec = self.claused_spec(name)?;
        let opts = ScalarQueryOpts { filters, ..ScalarQueryOpts::default() };
        let r = self.claused_resolve(name, &spec, &opts)?;
        let mut total = 0u64;
        #[cfg(not(target_arch = "wasm32"))]
        let probe = self.usage_cell(name);
        self.for_each_segment_windowed(name, |spec, seg, win| {
            total += seg.count_claused(min, max, &r.filters);
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(w) = win {
                crate::ops_index::advise::probe_window(&probe, w, min);
            }
            // The evicted half counts from the cold payloads — same
            // predicates, frozen values; a corrupt segment refuses.
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(w) = win.filter(|w| w.has_cold()) {
                total += w
                    .cold_claused_count(spec.ty(), min, max, &r.filters)
                    .map_err(|e| KevyError::Io(std::io::Error::other(e)))?;
            }
            #[cfg(target_arch = "wasm32")]
            let _ = (spec, win);
            Ok(())
        })?;
        self.observe_hit(name);
        Ok(total)
    }

    /// The named index's spec, feeding the advise log (a Range
    /// family) when the name is not declared.
    fn claused_spec(&self, name: &[u8]) -> KevyResult<IndexSpec> {
        let spec = {
            let g = self.indexes.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.1.get(name).map(|(s, _)| s.clone())
        };
        spec.ok_or_else(|| {
            self.observe_refused(name, kevy_index::AdviseShape::Range);
            KevyError::NotFound("no such index".into())
        })
    }

    /// [`resolve`], feeding the advise log when a clause named a
    /// field the index does not store.
    fn claused_resolve(
        &self,
        name: &[u8],
        spec: &IndexSpec,
        opts: &ScalarQueryOpts<'_>,
    ) -> KevyResult<Resolved> {
        resolve(spec, opts).inspect_err(|_| {
            if let Some(f) = unstored_field(spec, opts) {
                self.observe_refused(name, kevy_index::AdviseShape::Filter(f));
            }
        })
    }

    /// Every shard's claused page, unmerged, with the facet buckets
    /// folded by identity as the shards report them.
    fn gather_claused(
        &self,
        name: &[u8],
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
        clauses: &ScalarClauses<'_>,
    ) -> KevyResult<GatheredPages> {
        let mut all: Vec<(ScalarHit, ())> = Vec::new();
        let mut facets: Vec<Vec<FacetBucket>> = vec![Vec::new(); clauses.facets.len()];
        let mut found = false;
        #[cfg(not(target_arch = "wasm32"))]
        let probe = self.usage_cell(name);
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_spec, seg)) = inner.idx_segs.segs.iter().find(|(s, _)| s.name() == name) {
                found = true;
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(w) = inner.idx_segs.window_of(name) {
                    crate::ops_index::advise::probe_window(&probe, w, min);
                }
                let page = seg.query_claused(min, max, cursor, clauses);
                all.extend(page.hits.into_iter().map(|h| (h, ())));
                fold_facets(&mut facets, page.facets);
                // The shard's evicted half joins the union — the
                // origin merge below re-orders, re-collapses and
                // truncates the lot, so cold hits need no shard-level
                // pre-merge here.
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(w) = inner.idx_segs.window_of(name).filter(|w| w.has_cold()) {
                    let (chits, cfacets) = w
                        .cold_claused(_spec.ty(), min, max, cursor, clauses)
                        .map_err(|e| KevyError::Io(std::io::Error::other(e)))?;
                    all.extend(chits.into_iter().map(|h| (h, ())));
                    fold_facets(&mut facets, cfacets);
                }
            }
        }
        if !found {
            return Err(KevyError::NotFound("no such index".into()));
        }
        Ok((all, facets))
    }
}
