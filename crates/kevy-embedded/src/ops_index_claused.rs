//! The clause-carrying scalar query for the embedded API — `FILTER` /
//! `SORT` / `DISTINCT` / `FACET` / `OFFSET` on Range/Unique indexes
//! (the capacity arc's G1), plus [`ValueFilter`], the predicate shape
//! this surface shares with the text MATCH clauses. A `#[path]` child
//! of `ops_index.rs`, feature-independent of `text`.

use kevy_index::{
    Cursor, FacetBucket, IndexSpec, IndexValue, ScalarClauses, ScalarHit, ValType, ValueTest,
    fold_facets, merge_claused, sort_facets,
};

use super::sync_segs;
use crate::store::{Store, lock_write};
use crate::{KevyError, KevyResult};

/// One `FILTER` predicate: which stored value field it reads, and the
/// test on it — the wire's `RANGE` / `EQ` shapes, in-process.
///
/// The bounds are raw bytes and are coerced with the type the field was
/// DECLARED as, so a numeric range compares numerically rather than
/// lexicographically.
#[derive(Clone, Copy)]
pub enum ValueFilter<'a> {
    /// `field` between `min` and `max`, both inclusive.
    Range {
        /// The declared value field to read.
        field: &'a [u8],
        /// Lower bound, inclusive.
        min: &'a [u8],
        /// Upper bound, inclusive.
        max: &'a [u8],
    },
    /// `field` exactly `value`.
    Eq {
        /// The declared value field to read.
        field: &'a [u8],
        /// The value to match.
        value: &'a [u8],
    },
}

impl ValueFilter<'_> {
    pub(crate) fn field(&self) -> &[u8] {
        match self {
            ValueFilter::Range { field, .. } | ValueFilter::Eq { field, .. } => field,
        }
    }
}

/// One `FILTER` predicate resolved against the spec: the stored-value
/// position it reads, and the test built with that field's DECLARED type.
pub(crate) fn value_test(
    spec: &IndexSpec,
    f: &ValueFilter<'_>,
) -> KevyResult<(usize, ValueTest)> {
    let stored: Vec<&[u8]> = spec.values.iter().map(|v| v.name.as_slice()).collect();
    let pos = spec
        .values
        .iter()
        .position(|v| v.name == f.field())
        .ok_or_else(|| unknown_field("FILTER", f.field(), "store", &stored))?;
    let ty = spec.values[pos].ty;
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
    let stored = |f: &[u8]| spec.values.iter().any(|v| v.name == f);
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
        "{clause} names field '{}', which this index does not {verb} — it {verb}es: {}",
        String::from_utf8_lossy(bad),
        names.join(", ")
    ))
}

/// Everything a scalar RANGE/EQ query carries beyond its bounds and
/// limit — the embedded twin of the wire's optional clauses.
/// [`ScalarQueryOpts::default`] is the plain query.
#[derive(Clone, Copy, Default)]
pub struct ScalarQueryOpts<'a> {
    /// `FILTER …`: non-scoring predicates over stored values, ANDed. A
    /// row without the stored value fails — absent is not a value.
    pub filters: &'a [ValueFilter<'a>],
    /// `SORT <field> ASC|DESC`: order the page by a stored value; a row
    /// with no usable value sorts last in both directions.
    pub sort: Option<(&'a [u8], bool)>,
    /// `DISTINCT <field>`: at most one row per coerced value; a row
    /// with no value is its own group.
    pub distinct: Option<&'a [u8]>,
    /// `FACET <field…>`: count each field's values over the whole match
    /// set (FILTER reduces the counts; DISTINCT does not).
    pub facets: &'a [Vec<u8>],
    /// `OFFSET n`: rows to skip before `limit` takes effect.
    pub offset: usize,
}

impl ScalarQueryOpts<'_> {
    /// Whether any clause reshapes the selection (the cursor-refusing
    /// set — `FILTER` alone pages fine).
    pub(crate) fn selects(&self) -> bool {
        self.sort.is_some()
            || self.distinct.is_some()
            || !self.facets.is_empty()
            || self.offset > 0
    }
}

/// A clause-carrying query's answer: the page, per requested `FACET`
/// field its `(value, count)` buckets, and — on the FILTER-with-cursor
/// path — the cursor to resume from.
#[derive(Debug)]
pub struct ScalarPage {
    /// The selected rows, in the page's order.
    pub rows: Vec<(Vec<u8>, IndexValue)>,
    /// One entry per requested facet field, most frequent first.
    pub facets: Vec<Vec<(Vec<u8>, u64)>>,
    /// Resume cursor (`None` under any selection clause, which refuses
    /// cursors at the wire and pages nothing here either).
    pub cursor: Option<Cursor>,
}

/// The unmerged union of the shards' pages plus the folded facet
/// partials (the payload slot is `()` — the wire's hydration rides only
/// the server's chunks).
type GatheredPages = (Vec<(ScalarHit, ())>, Vec<Vec<FacetBucket>>);

/// What the spec-dependent clauses resolve to.
struct Resolved {
    filters: Vec<(usize, ValueTest)>,
    sort: Option<(usize, bool, ValType)>,
    distinct: Option<(usize, ValType)>,
    facets: Vec<(usize, ValType)>,
}

/// A clause's named stored-value field position + declared type.
fn value_field(
    spec: &IndexSpec,
    clause: &str,
    field: &[u8],
) -> KevyResult<(usize, ValType)> {
    let stored: Vec<&[u8]> = spec.values.iter().map(|v| v.name.as_slice()).collect();
    let pos = spec
        .values
        .iter()
        .position(|v| v.name == field)
        .ok_or_else(|| unknown_field(clause, field, "store", &stored))?;
    Ok((pos, spec.values[pos].ty))
}

fn resolve(spec: &IndexSpec, opts: &ScalarQueryOpts<'_>) -> KevyResult<Resolved> {
    let filters =
        opts.filters.iter().map(|f| value_test(spec, f)).collect::<KevyResult<Vec<_>>>()?;
    let sort = match opts.sort {
        Some((field, desc)) => {
            let (pos, ty) = value_field(spec, "SORT", field)?;
            Some((pos, desc, ty))
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
        let spec = IndexSpec {
            name: name.to_vec(),
            prefix: prefix.to_vec(),
            fields: vec![kevy_index::FieldSpec::new(field.to_vec())],
            ty,
            kind,
            max_bytes: 0,
            ann: None,
            group_by: None,
            with_positions: false,
            values: values
                .iter()
                .map(|(n, t)| kevy_index::ValueSpec { name: n.to_vec(), ty: *t })
                .collect(),
            composite: None,
        };
        self.register_spec(spec)
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
        let clauses = ScalarClauses {
            filters: &r.filters,
            sort: r.sort,
            distinct: r.distinct,
            facets: &r.facets,
            fetch: limit + offset,
        };
        let (all, mut facets) = self.gather_claused(name, min, max, cursor, &clauses)?;
        let sort_desc = r.sort.map(|(_, desc, _)| desc);
        let all = merge_claused(all, sort_desc, r.distinct.is_some(), offset, limit);
        sort_facets(&mut facets);
        let next = (!opts.selects() && all.len() == limit)
            .then(|| all.last().map(|(h, ())| Cursor { value: h.value.clone(), key: h.key.clone() }))
            .flatten();
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
        self.for_each_segment_windowed(name, |spec, seg, win| {
            total += seg.count_claused(min, max, &r.filters);
            // The evicted half counts from the cold payloads — same
            // predicates, frozen values; a corrupt segment refuses.
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(w) = win.filter(|w| w.has_cold()) {
                total += w
                    .cold_claused_count(spec.ty, min, max, &r.filters)
                    .map_err(|e| KevyError::Io(std::io::Error::other(e)))?;
            }
            #[cfg(target_arch = "wasm32")]
            let _ = (spec, win);
            Ok(())
        })?;
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
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_spec, seg)) = inner.idx_segs.segs.iter().find(|(s, _)| s.name == name) {
                found = true;
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
                        .cold_claused(_spec.ty, min, max, cursor, clauses)
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
