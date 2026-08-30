//! What a query asks for, beyond its text and result limit — the clause
//! options and the shapes they answer in. A child module of `segment`
//! (declared via `#[path]`), re-exported from it, so the crate's public
//! surface is unchanged by the split.

use super::{CorpusStats, Filter, TextMatch};

/// An order to select the top hits by, other than the score.
///
/// The key function maps a stored value's raw bytes to an
/// order-preserving encoding, computed once per candidate; the segment
/// then compares bytes and never learns what a number is. `None` from it
/// means the document has no usable value for the field, which sorts
/// **last in both directions** — missing is not a value, and placing it
/// at one end or the other by direction would make "the oldest" and "the
/// newest" disagree about where the unknowns went.
#[derive(Clone, Copy)]
pub struct Sort<'a> {
    /// Which declared value field orders the result.
    pub field: usize,
    /// Descending when true.
    pub desc: bool,
    /// The order-preserving encoding of one stored value.
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

/// Collapse the page so only the best document per value of a stored
/// field appears.
///
/// The key is the value's *identity*, coerced — so `1` and `1.0` in a
/// field declared `f64` are one value rather than two. A document with no
/// value for the field is its own group: `DISTINCT` removes documents
/// shown to share a value, and one that has none has not been shown to
/// share anything.
#[derive(Clone, Copy)]
pub struct Distinct<'a> {
    /// Which declared value field identifies a group.
    pub field: usize,
    /// The identity of one stored value.
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

/// Count the values of a stored field over the whole match set.
///
/// Buckets are keyed by the value's *identity* — the same coerced key
/// `DISTINCT` groups by, so `1` and `1.0` in a field declared `f64` are
/// one bucket — while the reported label is a spelling that really occurs
/// in the corpus rather than a re-serialisation.
#[derive(Clone, Copy)]
pub struct Facet<'a> {
    /// Which declared value field to count.
    pub field: usize,
    /// The identity of one stored value.
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

/// One value bucket: the identity a cross-shard merge sums by, a spelling
/// of it that occurs in the corpus, and how many documents matched with
/// it.
pub type Bucket = (Vec<u8>, Vec<u8>, u64);

/// One faceted query's answer: the page, and a count per value for each
/// requested field.
pub struct FacetedMatches {
    /// The ranked page, exactly what an unfaceted query would return.
    pub hits: Vec<TextMatch>,
    /// Per requested facet field, `(identity, label, count)` over the
    /// whole match set. The identity is what a cross-shard merge sums by;
    /// the label is what it reports.
    pub facets: Vec<Vec<Bucket>>,
}

/// Everything a MATCH query carries beyond its text and result limit.
///
/// Grouping them keeps the query entry point from growing a parameter per
/// clause, and gives every clause one place to be defaulted from
/// ([`QueryOpts::default`] is the plain, exact, unscoped query).
#[derive(Clone, Copy, Default)]
pub struct QueryOpts<'a> {
    /// Corpus-wide BM25 statistics — the second pass of a cross-shard
    /// query. `None` scores against this segment's own slice.
    pub stats: Option<&'a CorpusStats>,
    /// Edit distance allowed on bare terms (`TYPO n`); 0 = exact.
    pub typo: u32,
    /// Field positions the query is restricted to (`IN <field…>`); empty
    /// = every field.
    pub fields: &'a [usize],
    /// `FILTER`: non-scoring predicates, ANDed. Applied before the top-K
    /// — filtering afterwards would return fewer hits than exist.
    pub filter: &'a [Filter<'a>],
    /// `SORT`: select by a stored value instead of by score. Selecting,
    /// not re-ordering: a document that wins on the sort key must be
    /// chosen even when its score would never have reached the page.
    pub sort: Option<Sort<'a>>,
    /// `DISTINCT`: at most one hit per value of a stored field. Applied
    /// during selection, so the page is filled with `limit` DISTINCT
    /// documents rather than `limit` documents that then collapse.
    pub distinct: Option<Distinct<'a>>,
}
