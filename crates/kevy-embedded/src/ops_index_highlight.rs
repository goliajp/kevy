//! Clause-carrying MATCH for the embedded API, split from
//! `ops_index.rs` for the 500-LOC house rule. A `#[path]` child module,
//! so it reaches `Store`'s crate-private index methods and fields.

use kevy_index::IndexSpec;

use super::{FieldSpans, HighlightedHit, sync_segs};
use crate::store::{Store, lock_write};
use crate::{KevyError, KevyResult};

/// Everything a text MATCH carries beyond its index, query text and
/// result limit — the embedded twin of the wire's optional clauses.
///
/// Grouping them keeps one entry point instead of one per clause, and
/// [`MatchOpts::default`] is the plain query, so a caller opts into
/// exactly the clauses it names.
#[derive(Clone, Copy, Default)]
pub struct MatchOpts<'a> {
    /// `HIGHLIGHT`: `None` = not requested, `Some(&[])` = every indexed
    /// field, `Some(names)` = only those.
    pub highlight: Option<&'a [Vec<u8>]>,
    /// `TYPO n`: edit budget for each bare term; 0 = exact.
    pub typo: u32,
    /// `OFFSET n`: hits to skip before `limit` takes effect.
    pub offset: usize,
    /// `IN <field…>`: the declared field names to score within; empty =
    /// the whole document.
    pub scope: &'a [Vec<u8>],
    /// `FILTER …`: non-scoring predicates over stored values, ANDed.
    /// They decide which documents are eligible, not what a term is
    /// worth, so the corpus statistics stay whole-corpus.
    pub filters: &'a [ValueFilter<'a>],
    /// `SORT <field> ASC|DESC`: select by a stored value instead of by
    /// score. Selecting, not re-ordering — a document that wins on the
    /// key is chosen even when its score would never have reached the
    /// page.
    pub sort: Option<(&'a [u8], bool)>,
    /// `DISTINCT <field>`: at most one hit per value of a stored field,
    /// applied during selection so the page holds `limit` distinct
    /// documents rather than `limit` that then collapse.
    pub distinct: Option<&'a [u8]>,
}

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
    fn field(&self) -> &[u8] {
        match self {
            ValueFilter::Range { field, .. } | ValueFilter::Eq { field, .. } => field,
        }
    }
}

impl Store {
    /// [`Self::idx_match`] with every optional clause: highlight spans,
    /// a typo budget, an offset, and a field scope.
    ///
    /// A scoped query is a field-scoped BM25 — frequency, length and
    /// document frequency all come from the named fields alone — so
    /// naming a field the index does not declare is an error rather than
    /// an empty result that would look like a working query.
    pub fn idx_match_with(
        &self,
        name: &[u8],
        query: &[u8],
        limit: usize,
        opts: MatchOpts<'_>,
    ) -> KevyResult<Vec<HighlightedHit>> {
        let limit = limit.clamp(1, 1000);
        let offset = opts.offset.min(10_000);
        // Fetch deep enough to skip OFFSET and still fill LIMIT after the
        // cross-shard merge.
        let fetch = limit + offset;
        let (scope, tests) = self.resolve_clauses(name, opts.scope, opts.filters)?;
        let sorted = self.sort_field(name, opts.sort)?;
        let grouped = self.value_field("DISTINCT", name, opts.distinct)?;
        let dkey = grouped.map(|(_, ty)| move |raw: &[u8]| kevy_index::order_key(ty, raw));
        let distinct = grouped
            .zip(dkey.as_ref())
            .map(|((field, _), k)| kevy_text::Distinct { field, key: k });
        let key = sorted.map(|(_, _, ty)| move |raw: &[u8]| kevy_index::order_key(ty, raw));
        let sort = sorted
            .zip(key.as_ref())
            .map(|((field, desc, _), k)| kevy_text::Sort { field, desc, key: k });
        let boxed = box_tests(tests);
        let filter: Vec<kevy_text::Filter> = boxed
            .iter()
            .map(|(f, t)| kevy_text::Filter { field: *f, test: t.as_ref() })
            .collect();
        let stats = self.text_corpus_stats_in(name, query, opts.typo, &scope)?;
        let q = kevy_text::QueryOpts {
            stats: Some(&stats),
            typo: opts.typo,
            fields: &scope,
            filter: &filter,
            sort,
            distinct,
        };
        let mut all = self.gather_hits(name, query, fetch, q, opts.highlight);
        self.order_page(name, &mut all, sorted);
        self.collapse_union(name, &mut all, grouped);
        if offset > 0 {
            all.drain(..offset.min(all.len()));
        }
        all.truncate(limit);
        Ok(all)
    }

    /// Resolve the clauses that need the index spec: `IN` names onto
    /// field positions, `FILTER` predicates onto stored-value positions
    /// and typed tests.
    ///
    /// One catalog read for both, and both fail loudly on a name the
    /// index does not offer — an unknown field could just as easily match
    /// nothing, but then a typo would return a result indistinguishable
    /// from a working query with no hits.
    fn resolve_clauses(
        &self,
        name: &[u8],
        scope: &[Vec<u8>],
        filters: &[ValueFilter<'_>],
    ) -> KevyResult<ResolvedClauses> {
        if scope.is_empty() && filters.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let guard = self.indexes.catalog.read().unwrap_or_else(|e| e.into_inner());
        let Some((spec, _)) = guard.1.get(name) else {
            return Err(KevyError::NotFound("no such text index".into()));
        };
        let mut positions = Vec::with_capacity(scope.len());
        for want in scope {
            let names = || spec.fields.iter().map(|f| f.name.as_slice()).collect::<Vec<_>>();
            let i = spec
                .fields
                .iter()
                .position(|f| f.name == *want)
                .ok_or_else(|| unknown_field("IN", want, "index", &names()))?;
            positions.push(i);
        }
        let tests =
            filters.iter().map(|f| value_test(spec, f)).collect::<KevyResult<Vec<_>>>()?;
        Ok((positions, tests))
    }
}

/// What the spec-dependent clauses resolve to: `IN`'s field positions,
/// and `FILTER`'s (stored-value position, typed test) pairs.
type ResolvedClauses = (Vec<usize>, Vec<(usize, kevy_index::ValueTest)>);

/// Each resolved test boxed as the closure the segment takes. The boxes
/// must outlive the borrowed `Filter` list, so they are returned rather
/// than built inline.
type ValuePred = Box<dyn Fn(&[u8]) -> bool>;

fn box_tests(tests: Vec<(usize, kevy_index::ValueTest)>) -> Vec<(usize, ValuePred)> {
    tests
        .into_iter()
        .map(|(f, t)| {
            let b: ValuePred = Box::new(move |v: &[u8]| t.passes(v));
            (f, b)
        })
        .collect()
}

/// One `FILTER` predicate resolved against the spec: the stored-value
/// position it reads, and the test built with that field's DECLARED type.
fn value_test(
    spec: &IndexSpec,
    f: &ValueFilter<'_>,
) -> KevyResult<(usize, kevy_index::ValueTest)> {
    let stored: Vec<&[u8]> = spec.values.iter().map(|v| v.name.as_slice()).collect();
    let pos = spec
        .values
        .iter()
        .position(|v| v.name == f.field())
        .ok_or_else(|| unknown_field("FILTER", f.field(), "store", &stored))?;
    let ty = spec.values[pos].ty;
    let (test, raw) = match f {
        ValueFilter::Range { min, max, .. } => (kevy_index::ValueTest::range(ty, min, max), *min),
        ValueFilter::Eq { value, .. } => (kevy_index::ValueTest::eq(ty, value), *value),
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

impl Store {
    /// Every shard's page for this query, unmerged.
    fn gather_hits(
        &self,
        name: &[u8],
        query: &[u8],
        fetch: usize,
        q: kevy_text::QueryOpts<'_>,
        highlight: Option<&[Vec<u8>]>,
    ) -> Vec<HighlightedHit> {
        let mut all = Vec::new();
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((spec, ts)) = inner.idx_segs.text.iter().find(|(s, _)| s.name == name) {
                // `matches_query_with` parses quoted phrases out of the
                // raw query text; with none it is the ordinary term query.
                for m in ts.matches_query_with(query, fetch, q) {
                    let hl = highlight
                        .map_or_else(Vec::new, |w| hit_highlight(ts, spec, &m.key, query, w));
                    all.push((m.key, m.score, hl));
                }
            }
        }
        all
    }

    /// Put the merged hits in the page's order: by the sort key when the
    /// query gave one — the same definition each shard selected by — else
    /// by score.
    fn order_page(
        &self,
        name: &[u8],
        all: &mut Vec<HighlightedHit>,
        sorted: Option<(usize, bool, kevy_index::ValType)>,
    ) {
        let Some((field, desc, ty)) = sorted else {
            all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            return;
        };
        let mut keyed: Vec<(Option<Vec<u8>>, HighlightedHit)> = std::mem::take(all)
            .into_iter()
            .map(|h| (self.stored_order_key(name, &h.0, field, ty), h))
            .collect();
        keyed.sort_by(|a, b| {
            kevy_text::sorted_order((a.0.as_deref(), &a.1.0), (b.0.as_deref(), &b.1.0), desc)
        });
        *all = keyed.into_iter().map(|(_, h)| h).collect();
    }

    /// Collapse the union of the shards' pages: two shards can each hold
    /// a document with the same value, and only the better survives.
    ///
    /// `all` is already in the page's order, so the first occurrence of a
    /// value is its best and a stable retain keeps exactly that.
    /// Documents with no value are their own group and all survive — the
    /// same rule each shard collapsed by.
    fn collapse_union(
        &self,
        name: &[u8],
        all: &mut Vec<HighlightedHit>,
        grouped: Option<(usize, kevy_index::ValType)>,
    ) {
        let Some((field, ty)) = grouped else { return };
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        all.retain(|h| match self.stored_order_key(name, &h.0, field, ty) {
            Some(k) => seen.insert(k),
            None => true,
        });
    }

    /// Resolve a `SORT <field> ASC|DESC` clause to the stored-value
    /// position, the direction, and that field's declared type.
    fn sort_field(
        &self,
        name: &[u8],
        sort: Option<(&[u8], bool)>,
    ) -> KevyResult<Option<(usize, bool, kevy_index::ValType)>> {
        let Some((field, desc)) = sort else { return Ok(None) };
        let Some((pos, ty)) = self.value_field("SORT", name, Some(field))? else {
            return Ok(None);
        };
        Ok(Some((pos, desc, ty)))
    }

    /// A clause's named stored-value field, as a position and its
    /// declared type. Errors naming what the index does store.
    fn value_field(
        &self,
        clause: &str,
        name: &[u8],
        field: Option<&[u8]>,
    ) -> KevyResult<Option<(usize, kevy_index::ValType)>> {
        let Some(field) = field else { return Ok(None) };
        let guard = self.indexes.catalog.read().unwrap_or_else(|e| e.into_inner());
        let Some((spec, _)) = guard.1.get(name) else {
            return Err(KevyError::NotFound("no such text index".into()));
        };
        let stored: Vec<&[u8]> = spec.values.iter().map(|v| v.name.as_slice()).collect();
        let pos = spec
            .values
            .iter()
            .position(|v| v.name == field)
            .ok_or_else(|| unknown_field(clause, field, "store", &stored))?;
        Ok(Some((pos, spec.values[pos].ty)))
    }

    /// One row's sort key: its stored value, in the order-preserving
    /// encoding of that field's declared type.
    fn stored_order_key(
        &self,
        name: &[u8],
        key: &[u8],
        field: usize,
        ty: kevy_index::ValType,
    ) -> Option<Vec<u8>> {
        for shard in self.shards.iter() {
            let g = lock_write(shard);
            if let Some((_, ts)) = g.idx_segs.text.iter().find(|(s, _)| s.name == name)
                && let Some(raw) = ts.stored_value(key, field)
            {
                return kevy_index::order_key(ty, raw);
            }
        }
        None
    }
}

/// A clause naming a field the index does not offer, saying what it does.
fn unknown_field(clause: &str, bad: &[u8], verb: &str, offered: &[&[u8]]) -> KevyError {
    let names: Vec<String> =
        offered.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect();
    KevyError::InvalidInput(format!(
        "{clause} names field '{}', which this index does not {verb} — it {verb}es: {}",
        String::from_utf8_lossy(bad),
        names.join(", ")
    ))
}

/// One hit's highlight spans as `(field name, [(start, end)])`, filtered
/// to the requested fields (`want` empty = every field with a match).
fn hit_highlight(
    ts: &kevy_text::TextSegment,
    spec: &IndexSpec,
    key: &[u8],
    query: &[u8],
    want: &[Vec<u8>],
) -> Vec<FieldSpans> {
    ts.highlight_spans(key, query)
        .into_iter()
        .filter_map(|(fi, spans)| {
            let name = spec.fields.get(fi)?.name.clone();
            if !want.is_empty() && !want.contains(&name) {
                return None;
            }
            let ranges = spans.into_iter().map(|(s, e)| (s as u32, e as u32)).collect();
            Some((name, ranges))
        })
        .collect()
}
