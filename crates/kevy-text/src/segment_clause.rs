//! The value-reading clauses a query can carry — `FILTER`, `SORT`,
//! `DISTINCT` and `FACET` — each a field position plus a caller-supplied
//! function over that field's raw bytes. A child module of `segment`
//! (declared via `#[path]`), re-exported from it.

use super::SortOrder;

/// A non-scoring predicate over a document's stored values.
///
/// The test takes raw bytes because this crate does not know what a
/// number or a date is; the caller coerces. A document with no value for
/// the field never passes — absent is not a value.
///
/// ```
/// let even = |v: &[u8]| v.last().is_some_and(|b| b % 2 == 0);
/// let f = kevy_text::Filter::new(0, &even);
/// assert!((f.test)(b"2") && !(f.test)(b"3"));
/// ```
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Filter<'a> {
    /// Which declared value field the predicate reads.
    ///
    /// ```
    /// use kevy_text::{Filter, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(2));
    /// // value 0 = color, value 1 = size
    /// seg.apply_doc(b"a", Some(&[(b"wool hat".to_vec(), 1.0)]), &[Some(b"red"), Some(b"L")]);
    /// seg.apply_doc(b"b", Some(&[(b"wool sock".to_vec(), 1.0)]), &[Some(b"blue"), Some(b"S")]);
    /// let is_small = |v: &[u8]| v == b"S";
    /// let filter = [Filter::new(1, &is_small)];
    /// let hits = seg.matches_query_with(b"wool", 10, QueryOpts::default().with_filter(&filter));
    /// assert_eq!(hits.len(), 1);
    /// assert_eq!(hits[0].key, b"b");
    /// ```
    pub field: usize,
    /// The test applied to that field's bytes.
    ///
    /// A document with no value for the field never reaches the test:
    ///
    /// ```
    /// use kevy_text::{Filter, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// seg.apply_doc(b"priced", Some(&[(b"wool hat".to_vec(), 1.0)]), &[Some(b"12")]);
    /// seg.apply_doc(b"unpriced", Some(&[(b"wool sock".to_vec(), 1.0)]), &[None]);
    /// let anything = |_: &[u8]| true;
    /// let filter = [Filter::new(0, &anything)];
    /// let hits = seg.matches_query_with(b"wool", 10, QueryOpts::default().with_filter(&filter));
    /// assert_eq!(hits.len(), 1);
    /// assert_eq!(hits[0].key, b"priced");
    /// ```
    pub test: &'a dyn Fn(&[u8]) -> bool,
}

impl<'a> Filter<'a> {
    /// A predicate `test` over declared value field `field`.
    ///
    /// ```
    /// let any = |_: &[u8]| true;
    /// assert_eq!(kevy_text::Filter::new(2, &any).field, 2);
    /// ```
    #[must_use]
    pub fn new(field: usize, test: &'a dyn Fn(&[u8]) -> bool) -> Self {
        Self { field, test }
    }
}

/// An order to select the top hits by, other than the score.
///
/// The key function maps a stored value's raw bytes to an
/// order-preserving encoding, computed once per candidate; the segment
/// then compares bytes and never learns what a number is. `None` from it
/// means the document has no usable value for the field, which sorts
/// **last in both directions** — missing is not a value, and placing it
/// at one end or the other by direction would make "the oldest" and "the
/// newest" disagree about where the unknowns went.
///
/// ```
/// use kevy_text::{Sort, SortOrder};
/// let key = |v: &[u8]| Some(v.to_vec());
/// let s = Sort::new(0, &key).with_order(SortOrder::Desc);
/// assert_eq!((s.field, s.order), (0, SortOrder::Desc));
/// ```
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Sort<'a> {
    /// Which declared value field orders the result.
    ///
    /// ```
    /// use kevy_text::{QueryOpts, SegmentShape, Sort, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(2));
    /// // value 0 = name, value 1 = year
    /// seg.apply_doc(b"a", Some(&[(b"red".to_vec(), 1.0)]), &[Some(b"alpha"), Some(b"2020")]);
    /// seg.apply_doc(b"b", Some(&[(b"red".to_vec(), 1.0)]), &[Some(b"beta"), Some(b"1990")]);
    /// let raw = |v: &[u8]| Some(v.to_vec());
    /// let by_name = QueryOpts::default().with_sort(Sort::new(0, &raw));
    /// let by_year = QueryOpts::default().with_sort(Sort::new(1, &raw));
    /// assert_eq!(seg.matches_query_with(b"red", 1, by_name)[0].key, b"a");
    /// assert_eq!(seg.matches_query_with(b"red", 1, by_year)[0].key, b"b");
    /// ```
    pub field: usize,
    /// Which way it orders.
    ///
    /// ```
    /// use kevy_text::{QueryOpts, SegmentShape, Sort, SortOrder, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// seg.apply_doc(b"old", Some(&[(b"red".to_vec(), 1.0)]), &[Some(b"1990")]);
    /// seg.apply_doc(b"new", Some(&[(b"red".to_vec(), 1.0)]), &[Some(b"2020")]);
    /// let year = |v: &[u8]| Some(v.to_vec());
    /// let mut sort = Sort::new(0, &year);
    /// sort.order = SortOrder::Desc;
    /// let hits = seg.matches_query_with(b"red", 1, QueryOpts::default().with_sort(sort));
    /// assert_eq!(hits[0].key, b"new");
    /// ```
    pub order: SortOrder,
    /// The order-preserving encoding of one stored value.
    ///
    /// Numbers stored as text do not sort as text; the key turns them
    /// into bytes that do, and `None` sends a value it cannot read last:
    ///
    /// ```
    /// use kevy_text::{QueryOpts, SegmentShape, Sort, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (k, price) in [("nine", "9"), ("ten", "10"), ("junk", "n/a")] {
    ///     seg.apply_doc(k.as_bytes(), Some(&[(b"red".to_vec(), 1.0)]), &[Some(price.as_bytes())]);
    /// }
    /// let numeric = |v: &[u8]| {
    ///     let n: u64 = std::str::from_utf8(v).ok()?.parse().ok()?;
    ///     Some(n.to_be_bytes().to_vec())
    /// };
    /// let page = seg.matches_query_with(b"red", 3, QueryOpts::default().with_sort(Sort::new(0, &numeric)));
    /// let keys: Vec<&[u8]> = page.iter().map(|m| &m.key[..]).collect();
    /// assert_eq!(keys, [&b"nine"[..], b"ten", b"junk"]);
    /// ```
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

impl<'a> Sort<'a> {
    /// Ascending by field `field`, encoded by `key`.
    ///
    /// ```
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// assert_eq!(kevy_text::Sort::new(1, &key).order, kevy_text::SortOrder::Asc);
    /// ```
    #[must_use]
    pub fn new(field: usize, key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>) -> Self {
        Self { field, order: SortOrder::Asc, key }
    }

    /// The same sort, running `order`.
    ///
    /// ```
    /// use kevy_text::{Sort, SortOrder};
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// assert_eq!(Sort::new(1, &key).with_order(SortOrder::Desc).order, SortOrder::Desc);
    /// ```
    #[must_use]
    pub fn with_order(mut self, order: SortOrder) -> Self {
        self.order = order;
        self
    }
}

/// Collapse the page so only the best document per value of a stored
/// field appears.
///
/// The key is the value's *identity*, coerced — so `1` and `1.0` in a
/// field declared `f64` are one value rather than two. A document with no
/// value for the field is its own group: `DISTINCT` removes documents
/// shown to share a value, and one that has none has not been shown to
/// share anything.
///
/// ```
/// let key = |v: &[u8]| Some(v.to_vec());
/// assert_eq!(kevy_text::Distinct::new(3, &key).field, 3);
/// ```
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Distinct<'a> {
    /// Which declared value field identifies a group.
    ///
    /// ```
    /// use kevy_text::{Distinct, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(2));
    /// // value 0 = brand, value 1 = color
    /// seg.apply_doc(b"a", Some(&[(b"red hat".to_vec(), 1.0)]), &[Some(b"acme"), Some(b"red")]);
    /// seg.apply_doc(b"b", Some(&[(b"red cap".to_vec(), 1.0)]), &[Some(b"zeta"), Some(b"red")]);
    /// let raw = |v: &[u8]| Some(v.to_vec());
    /// let per_brand = QueryOpts::default().with_distinct(Distinct::new(0, &raw));
    /// let per_color = QueryOpts::default().with_distinct(Distinct::new(1, &raw));
    /// assert_eq!(seg.matches_query_with(b"red", 10, per_brand).len(), 2);
    /// assert_eq!(seg.matches_query_with(b"red", 10, per_color).len(), 1);
    /// ```
    pub field: usize,
    /// The identity of one stored value.
    ///
    /// Values the key maps to the same bytes are one group:
    ///
    /// ```
    /// use kevy_text::{Distinct, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// seg.apply_doc(b"a", Some(&[(b"red hat".to_vec(), 1.0)]), &[Some(b"ACME")]);
    /// seg.apply_doc(b"b", Some(&[(b"red cap".to_vec(), 1.0)]), &[Some(b"acme")]);
    /// let exact = |v: &[u8]| Some(v.to_vec());
    /// let folded = |v: &[u8]| Some(v.to_ascii_lowercase());
    /// let q = |key| QueryOpts::default().with_distinct(Distinct::new(0, key));
    /// assert_eq!(seg.matches_query_with(b"red", 10, q(&exact)).len(), 2);
    /// assert_eq!(seg.matches_query_with(b"red", 10, q(&folded)).len(), 1);
    /// ```
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

impl<'a> Distinct<'a> {
    /// One hit per identity `key` gives field `field`.
    ///
    /// ```
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// assert_eq!(kevy_text::Distinct::new(0, &key).field, 0);
    /// ```
    #[must_use]
    pub fn new(field: usize, key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>) -> Self {
        Self { field, key }
    }
}

/// Count the values of a stored field over the whole match set.
///
/// Buckets are keyed by the value's *identity* — the same coerced key
/// `DISTINCT` groups by, so `1` and `1.0` in a field declared `f64` are
/// one bucket — while the reported label is a spelling that really occurs
/// in the corpus rather than a re-serialisation.
///
/// ```
/// let key = |v: &[u8]| Some(v.to_ascii_lowercase());
/// assert_eq!(kevy_text::Facet::new(1, &key).field, 1);
/// ```
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Facet<'a> {
    /// Which declared value field to count.
    ///
    /// ```
    /// use kevy_text::{Facet, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(2));
    /// // value 0 = brand, value 1 = color
    /// seg.apply_doc(b"a", Some(&[(b"red hat".to_vec(), 1.0)]), &[Some(b"acme"), Some(b"red")]);
    /// seg.apply_doc(b"b", Some(&[(b"red cap".to_vec(), 1.0)]), &[Some(b"zeta"), Some(b"red")]);
    /// let raw = |v: &[u8]| Some(v.to_vec());
    /// let r = seg.matches_query_faceted(b"red", 10, QueryOpts::default(), &[Facet::new(1, &raw)]);
    /// assert_eq!(r.facets[0], [(b"red".to_vec(), b"red".to_vec(), 2)]);
    /// ```
    pub field: usize,
    /// The identity of one stored value.
    ///
    /// `None` leaves the document out of the counts:
    ///
    /// ```
    /// use kevy_text::{Facet, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (k, size) in [("a", "10"), ("b", "12"), ("c", "?")] {
    ///     seg.apply_doc(k.as_bytes(), Some(&[(b"shoe".to_vec(), 1.0)]), &[Some(size.as_bytes())]);
    /// }
    /// let numeric = |v: &[u8]| v.iter().all(u8::is_ascii_digit).then(|| v.to_vec());
    /// let r = seg.matches_query_faceted(b"shoe", 10, QueryOpts::default(), &[Facet::new(0, &numeric)]);
    /// let total: u64 = r.facets[0].iter().map(|b| b.2).sum();
    /// assert_eq!(total, 2);
    /// ```
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

impl<'a> Facet<'a> {
    /// Count field `field` by the identity `key` gives it.
    ///
    /// ```
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// assert_eq!(kevy_text::Facet::new(4, &key).field, 4);
    /// ```
    #[must_use]
    pub fn new(field: usize, key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>) -> Self {
        Self { field, key }
    }
}

impl core::fmt::Debug for Filter<'_> {
    /// Prints every field except `test`.
    ///
    /// The predicate is a `&dyn Fn`, which has no `Debug` and no stable
    /// identity worth printing — it shows as `<fn>` so the rest of the
    /// struct stays inspectable.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Filter").field("field", &self.field).field("test", &"<fn>").finish()
    }
}

impl core::fmt::Debug for Sort<'_> {
    /// Prints every field except `key`.
    ///
    /// The ordering key is a `&dyn Fn`, which has no `Debug` and no stable
    /// identity worth printing — it shows as `<fn>` so the rest of the
    /// struct stays inspectable.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sort")
            .field("field", &self.field)
            .field("order", &self.order)
            .field("key", &"<fn>")
            .finish()
    }
}

impl core::fmt::Debug for Distinct<'_> {
    /// Prints every field except `key`.
    ///
    /// The identity key is a `&dyn Fn`, which has no `Debug` and no stable
    /// identity worth printing — it shows as `<fn>` so the rest of the
    /// struct stays inspectable.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Distinct").field("field", &self.field).field("key", &"<fn>").finish()
    }
}

impl core::fmt::Debug for Facet<'_> {
    /// Prints every field except `key`.
    ///
    /// The bucketing key is a `&dyn Fn`, which has no `Debug` and no stable
    /// identity worth printing — it shows as `<fn>` so the rest of the
    /// struct stays inspectable.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Facet").field("field", &self.field).field("key", &"<fn>").finish()
    }
}
