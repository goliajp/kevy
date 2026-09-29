//! The two plain-data shapes around a [`TextSegment`](super::TextSegment):
//! what an index declares it to be built from, and the sizing counters
//! it reports. A child module of `segment` (declared via `#[path]`),
//! re-exported from it.

/// Sizing counters (memory formula + IDX.LIST).
///
/// ```
/// use kevy_text::{TextSegment, TextStats};
/// let mut seg = TextSegment::new();
/// assert_eq!(seg.stats(), TextStats::default());
/// seg.apply(b"doc:1", Some(b"red apple"));
/// seg.apply(b"doc:2", Some(b"green apple"));
/// let s = seg.stats();
/// assert_eq!((s.docs, s.tokens, s.postings), (2, 3, 4));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TextStats {
    /// Indexed documents.
    ///
    /// ```
    /// let mut seg = kevy_text::TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"red apple"));
    /// seg.apply(b"doc:1", Some(b"red pear"));
    /// // re-indexing a key replaces its document rather than adding one
    /// assert_eq!(seg.stats().docs, 1);
    /// ```
    pub docs: u64,
    /// Distinct tokens.
    ///
    /// ```
    /// let mut seg = kevy_text::TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"red apple"));
    /// seg.apply(b"doc:2", Some(b"green apple"));
    /// // red, apple, green
    /// assert_eq!(seg.stats().tokens, 3);
    /// ```
    pub tokens: u64,
    /// Total postings.
    ///
    /// One per (token, document) pair, however often the token repeats
    /// inside the document:
    ///
    /// ```
    /// let mut seg = kevy_text::TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"apple apple apple"));
    /// seg.apply(b"doc:2", Some(b"green apple"));
    /// assert_eq!(seg.stats().postings, 3);
    /// ```
    pub postings: u64,
    /// Approximate heap bytes (the measured side of the documented
    /// memory formula).
    ///
    /// ```
    /// let mut seg = kevy_text::TextSegment::new();
    /// assert_eq!(seg.stats().approx_bytes, 0);
    /// seg.apply(b"doc:1", Some(b"a document worth some bytes"));
    /// let one = seg.stats().approx_bytes;
    /// assert!(one > 0);
    /// seg.apply(b"doc:1", None);
    /// assert!(seg.stats().approx_bytes < one);
    /// ```
    pub approx_bytes: u64,
}

/// What an index declares, in the terms a segment is built from.
///
/// Start from [`SegmentShape::default`] (one field, no positions, no
/// values) and set what the index declares.
///
/// ```
/// let s = kevy_text::SegmentShape::default().with_fields(3).with_positions(true).with_values(2);
/// assert_eq!((s.fields, s.positions, s.values), (3, true, 2));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SegmentShape {
    /// Separately scored fields — `IN <field…>` scopes to these. 0 or 1
    /// keeps no per-field breakdown, because with one field the
    /// per-field numbers are the merged ones.
    ///
    /// ```
    /// use kevy_text::{QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_fields(2));
    /// assert_eq!(seg.field_arity(), 2);
    /// // field 0 is the title, field 1 the body
    /// seg.apply_fields(b"doc:1", Some(&[(b"rust".to_vec(), 1.0), (b"a book".to_vec(), 1.0)]));
    /// seg.apply_fields(b"doc:2", Some(&[(b"a book".to_vec(), 1.0), (b"rust".to_vec(), 1.0)]));
    /// let hits = seg.matches_query_with(b"rust", 10, QueryOpts::default().with_fields(&[0]));
    /// assert_eq!(hits.len(), 1);
    /// assert_eq!(hits[0].key, b"doc:1");
    /// ```
    pub fields: usize,
    /// Record token positions (`WITH POSITIONS`) for phrase, proximity
    /// and adjacency-verified highlight.
    ///
    /// ```
    /// use kevy_text::{SegmentShape, TextSegment};
    /// let mut plain = TextSegment::new();
    /// let mut pos = TextSegment::with_shape(SegmentShape::default().with_positions(true));
    /// for seg in [&mut plain, &mut pos] {
    ///     seg.apply(b"doc:1", Some(b"quick brown fox"));
    ///     seg.apply(b"doc:2", Some(b"brown quick fox"));
    /// }
    /// // only the segment that recorded positions can verify adjacency
    /// assert!(plain.phrase_matches(b"quick brown", 10, None).is_empty());
    /// let hits = pos.phrase_matches(b"quick brown", 10, None);
    /// assert_eq!(hits.len(), 1);
    /// assert_eq!(hits[0].key, b"doc:1");
    /// ```
    pub positions: bool,
    /// Value fields stored per document (`VALUES`), for the clauses that
    /// read a document's own value rather than a term's postings.
    ///
    /// ```
    /// use kevy_text::{SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// assert_eq!(seg.value_arity(), 1);
    /// seg.apply_doc(b"sku:1", Some(&[(b"red shoe".to_vec(), 1.0)]), &[Some(b"30")]);
    /// assert_eq!(seg.stored_value(b"sku:1", 0), Some(&b"30"[..]));
    /// ```
    pub values: usize,
}

impl SegmentShape {
    /// Set [`SegmentShape::fields`], the separately scored fields.
    ///
    /// ```
    /// assert_eq!(kevy_text::SegmentShape::default().with_fields(2).fields, 2);
    /// ```
    #[must_use]
    pub fn with_fields(mut self, fields: usize) -> Self {
        self.fields = fields;
        self
    }

    /// Set [`SegmentShape::positions`]: record token positions.
    ///
    /// ```
    /// assert!(kevy_text::SegmentShape::default().with_positions(true).positions);
    /// ```
    #[must_use]
    pub fn with_positions(mut self, positions: bool) -> Self {
        self.positions = positions;
        self
    }

    /// Set [`SegmentShape::values`], the value fields stored per document.
    ///
    /// ```
    /// assert_eq!(kevy_text::SegmentShape::default().with_values(1).values, 1);
    /// ```
    #[must_use]
    pub fn with_values(mut self, values: usize) -> Self {
        self.values = values;
        self
    }
}
