//! What pass 2 hands back from the cold directory: the hits in page
//! order, their frozen stored values and the facet counts.

use std::collections::HashMap;

/// One cold hit: its page-order ingredients, ready to merge.
///
/// ```
/// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
/// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
/// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
/// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
/// }
/// let mut cold = kevy_window::TextColdDir::new();
/// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
/// let stats = CorpusStats::new(3.0, 2.0, Default::default());
/// let page = cold.cold_page(&kevy_window::ColdPageQuery::parse(b"red", &stats, 10));
/// let hit = &page.hits[0];
/// assert_eq!(hit.key, b"d:1");
/// assert!(hit.score > 0.0);
/// assert_eq!(hit.okey, None); // no SORT clause
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ColdHit {
    /// The row key this hit points at.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let page = cold.cold_page(&kevy_window::ColdPageQuery::parse(b"fig", &stats, 10));
    /// assert!(page.hits.is_empty()); // d:3 is hot, so the cold side has no fig
    /// let page = cold.cold_page(&kevy_window::ColdPageQuery::parse(b"green", &stats, 10));
    /// assert_eq!(page.hits[0].key, b"d:2");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub key: Vec<u8>,
    /// Its BM25 relevance. Comparable across segments because the
    /// document-frequency corrections are applied before the merge, not
    /// after — a per-segment score would not be.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let page = cold.cold_page(&kevy_window::ColdPageQuery::parse(b"green apple", &stats, 10));
    /// // d:2 matches both terms and outranks d:1, which matches one
    /// assert!(page.hits[0].score > page.hits[1].score);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub score: f64,
    /// The sort key, when the query sorts by a stored value.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let by_colour = |v: &[u8]| Some(v.to_vec());
    /// let sort = kevy_text::Sort::new(0, &by_colour);
    /// let q = kevy_window::ColdPageQuery::parse(b"apple", &stats, 10).with_sort(&sort);
    /// assert_eq!(cold.cold_page(&q).hits[0].okey.as_deref(), Some(&b"green"[..]));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub okey: Option<Vec<u8>>,
}

/// The cold half of one shard's pass-2 answer.
///
/// ```
/// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
/// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
/// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
/// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
/// }
/// let mut cold = kevy_window::TextColdDir::new();
/// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
/// let stats = CorpusStats::new(3.0, 2.0, Default::default());
/// let colour = |v: &[u8]| Some(v.to_vec());
/// let facets = [kevy_text::Facet::new(0, &colour)];
/// let q = kevy_window::ColdPageQuery::parse(b"apple", &stats, 10).with_facets(&facets);
/// let page = cold.cold_page(&q);
/// assert_eq!((page.hits.len(), page.values.len(), page.facets.len()), (2, 2, 1));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ColdPage {
    /// Best `fetch` cold hits in the page's order.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let page = cold.cold_page(&kevy_window::ColdPageQuery::parse(b"apple", &stats, 1));
    /// assert_eq!(page.hits.len(), 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub hits: Vec<ColdHit>,
    /// The returned hits' frozen stored values — what the merge reads
    /// for sort/distinct identities and the origin's okeys/dkeys.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let is_red = |v: &[u8]| v == b"red";
    /// let filter = [kevy_text::Filter::new(0, &is_red)];
    /// let page = cold.cold_page(&kevy_window::ColdPageQuery::parse(b"apple", &stats, 10).with_filter(&filter));
    /// assert_eq!(page.values[&b"d:1".to_vec()], [Some(b"red".to_vec())]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub values: HashMap<Vec<u8>, Vec<Option<Vec<u8>>>>,
    /// Per requested facet field, (identity, label, count) over the
    /// filtered cold match set.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let colour = |v: &[u8]| Some(v.to_vec());
    /// let facets = [kevy_text::Facet::new(0, &colour)];
    /// let page = cold.cold_page(&kevy_window::ColdPageQuery::parse(b"apple", &stats, 10).with_facets(&facets));
    /// let mut counts: Vec<(Vec<u8>, u64)> = page.facets[0].iter().map(|b| (b.1.clone(), b.2)).collect();
    /// counts.sort();
    /// assert_eq!(counts, [(b"green".to_vec(), 1), (b"red".to_vec(), 1)]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub facets: Vec<Vec<kevy_text::Bucket>>,
}
