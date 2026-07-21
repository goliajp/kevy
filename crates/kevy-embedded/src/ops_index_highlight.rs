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
        let boxed = box_tests(tests);
        let filter: Vec<kevy_text::Filter> = boxed
            .iter()
            .map(|(f, t)| kevy_text::Filter { field: *f, test: t.as_ref() })
            .collect();
        let stats = self.text_corpus_stats_in(name, query, opts.typo, &scope)?;
        let mut all: Vec<HighlightedHit> = Vec::new();
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((spec, ts)) = inner.idx_segs.text.iter().find(|(s, _)| s.name == name) {
                // `matches_query_with` parses quoted phrases out of the
                // raw query text; with none it is the ordinary term query.
                let q = kevy_text::QueryOpts {
                    stats: Some(&stats),
                    typo: opts.typo,
                    fields: &scope,
                    filter: &filter,
                };
                for m in ts.matches_query_with(query, fetch, q) {
                    let hl = opts
                        .highlight
                        .map_or_else(Vec::new, |w| hit_highlight(ts, spec, &m.key, query, w));
                    all.push((m.key, m.score, hl));
                }
            }
        }
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
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
