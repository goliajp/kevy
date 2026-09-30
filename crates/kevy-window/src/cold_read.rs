//! The cold half of range and count reads: what a query merges in from
//! the sealed segments, shadowed entries skipped. A child of the crate
//! root, so it reads the window state's private shape directly.

use kevy_index::{
    ColdEntryRow, FacetBucket, IndexValue, ScalarClauses, ScalarHit, ValType, claused_over,
    decode_seg_key, decode_seg_values, seg_bounds, values_pass,
};

use crate::{ColdError, WindowRt};

impl WindowRt {
    /// Cold count of values in `[min, max]`: fast whole-segment
    /// arithmetic while no tombstones exist (the common state), a
    /// decode walk once any do. `Err` = a segment refused (corrupt
    /// derived spill) — the query reports it, never a partial number.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment, ValType, WindowShape, WindowSpec};
    /// # let dir = kevy_tmpdir::TmpDir::new("window-doc");
    /// let mut w = kevy_window::WindowRt::new(WindowSpec::new("ts", 100, 50), WindowShape::PlainI64);
    /// let mut seg = Segment::new();
    /// for ts in [10, 20, 300] {
    ///     seg.apply(format!("r:{ts}").as_bytes(), None, Some(IndexValue::I64(ts)));
    /// }
    /// assert!(w.slide(b"t.ts", &mut seg, dir.path())?);
    /// assert_eq!(w.cold_count(ValType::I64, &IndexValue::I64(0), &IndexValue::I64(15))?, 1);
    /// assert_eq!(w.cold_count(ValType::I64, &IndexValue::I64(0), &IndexValue::I64(1_000))?, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn cold_count(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
    ) -> Result<u64, ColdError> {
        let (lo, hi) = seg_bounds(min, max);
        if self.tombs.is_empty() {
            let mut n = 0u64;
            for (_, s) in &self.cold {
                n += s.count_range(&lo, &hi)?;
            }
            return Ok(n);
        }
        Ok(self.cold_hits(ty, min, max, None, usize::MAX)?.len() as u64)
    }

    /// Cold hits of `[min, max]` in value order, tombstones skipped
    /// and — when a page resumes — everything at or before `cursor`
    /// skipped BEFORE the limit counts, at most `limit`. (Counting
    /// first and filtering at the merge starves the cold side on any
    /// page after the first: the limit fills with pre-cursor entries
    /// that are then all dropped.) Segments hold disjoint ascending
    /// value ranges (each slide covers `[old_w, new_w)`), so chaining
    /// them in creation order IS value order. `Err` on a corrupt
    /// segment — never a silent partial page.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment, ValType, WindowShape, WindowSpec};
    /// # let dir = kevy_tmpdir::TmpDir::new("window-doc");
    /// let mut w = kevy_window::WindowRt::new(WindowSpec::new("ts", 100, 50), WindowShape::PlainI64);
    /// let mut seg = Segment::new();
    /// for ts in [10, 20, 300] {
    ///     seg.apply(format!("r:{ts}").as_bytes(), None, Some(IndexValue::I64(ts)));
    /// }
    /// assert!(w.slide(b"t.ts", &mut seg, dir.path())?);
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(1_000));
    /// let page = w.cold_hits(ValType::I64, &lo, &hi, None, 1)?;
    /// assert_eq!(page, [(b"r:10".to_vec(), IndexValue::I64(10))]);
    ///
    /// // resume after the last entry served
    /// let cur = kevy_index::Cursor::new(IndexValue::I64(10), b"r:10".to_vec());
    /// let next = w.cold_hits(ValType::I64, &lo, &hi, Some(&cur), 1)?;
    /// assert_eq!(next, [(b"r:20".to_vec(), IndexValue::I64(20))]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn cold_hits(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&kevy_index::Cursor>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, IndexValue)>, ColdError> {
        let (lo, hi) = seg_bounds(min, max);
        let mut out = Vec::new();
        for (seq, seg) in &self.cold {
            for r in seg.range(&lo, &hi) {
                let (k, _) = r?;
                let Some((v, row)) = decode_seg_key(ty, &k) else { continue };
                if self.shadowed(&row, *seq) {
                    continue;
                }
                if cursor.is_some_and(|c| (&v, row.as_slice()) <= (&c.value, c.key.as_slice())) {
                    continue;
                }
                out.push((row, v));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// The clause-carrying cold count: the FILTER predicates applied
    /// to each live cold entry's payload values. `Err` on a corrupt
    /// segment — the query reports it, never a partial number.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType, ValueTest, WindowShape, WindowSpec};
    /// # let dir = kevy_tmpdir::TmpDir::new("window-doc");
    /// let mut w = kevy_window::WindowRt::new(WindowSpec::new("ts", 100, 50), WindowShape::PlainI64);
    /// // one stored value per row: its status
    /// let mut seg = Segment::with_values(1);
    /// for (ts, status) in [(10, "paid"), (20, "open"), (30, "paid"), (300, "open")] {
    ///     let key = format!("r:{ts}");
    ///     seg.apply_with_values(key.as_bytes(), None, Some(IndexValue::I64(ts)), &[Some(status.as_bytes())]);
    /// }
    /// assert!(w.slide(b"t.ts", &mut seg, dir.path())?);
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(1_000));
    /// let paid = [(0, ValueTest::eq(ValType::Str, b"paid").expect("a test"))];
    /// assert_eq!(w.cold_claused_count(ValType::I64, &lo, &hi, &paid)?, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn cold_claused_count(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
        filters: &[(usize, kevy_index::ValueTest)],
    ) -> Result<u64, ColdError> {
        let mut n = 0u64;
        for (_, _, vals) in self.decode_range(ty, min, max, None)? {
            if values_pass(&vals, filters) {
                n += 1;
            }
        }
        Ok(n)
    }

    /// The clause-carrying cold page: every live cold entry in
    /// `[min, max]` (past `cursor` when one rides), decoded and fed to
    /// the shared clause walk — the same FILTER / SORT / DISTINCT /
    /// FACET semantics the hot tree runs, over the frozen payloads.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType, ValueTest, WindowShape, WindowSpec};
    /// # let dir = kevy_tmpdir::TmpDir::new("window-doc");
    /// let mut w = kevy_window::WindowRt::new(WindowSpec::new("ts", 100, 50), WindowShape::PlainI64);
    /// // one stored value per row: its status
    /// let mut seg = Segment::with_values(1);
    /// for (ts, status) in [(10, "paid"), (20, "open"), (30, "paid"), (300, "open")] {
    ///     let key = format!("r:{ts}");
    ///     seg.apply_with_values(key.as_bytes(), None, Some(IndexValue::I64(ts)), &[Some(status.as_bytes())]);
    /// }
    /// assert!(w.slide(b"t.ts", &mut seg, dir.path())?);
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(1_000));
    /// let facets = [(0, ValType::Str)];
    /// let c = ScalarClauses::new(10).with_sort(0, SortOrder::Asc, ValType::Str).with_facets(&facets);
    /// let (hits, buckets) = w.cold_claused(ValType::I64, &lo, &hi, None, &c)?;
    /// assert_eq!(hits[0].key, b"r:20"); // "open" sorts first
    /// assert_eq!(buckets[0].len(), 2); // two distinct statuses among the cold rows
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn cold_claused(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&kevy_index::Cursor>,
        c: &ScalarClauses<'_>,
    ) -> Result<(Vec<ScalarHit>, Vec<Vec<FacetBucket>>), ColdError> {
        let items = self.decode_range(ty, min, max, cursor)?;
        Ok(claused_over(items.into_iter(), c))
    }

    /// Every live cold entry of `[min, max]` past `cursor`, decoded to
    /// `(value, row_key, payload values)` in value order. `Err` on any
    /// malformed key or payload — corrupt derived spill refuses.
    fn decode_range(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&kevy_index::Cursor>,
    ) -> Result<Vec<ColdEntryRow>, ColdError> {
        let (lo, hi) = seg_bounds(min, max);
        let mut out = Vec::new();
        for (seq, seg) in &self.cold {
            for r in seg.range(&lo, &hi) {
                let (k, payload) = r?;
                let (v, row) = decode_seg_key(ty, &k).ok_or(ColdError::CorruptKey)?;
                if self.shadowed(&row, *seq) {
                    continue;
                }
                if cursor.is_some_and(|c| (&v, row.as_slice()) <= (&c.value, c.key.as_slice())) {
                    continue;
                }
                let vals = decode_seg_values(&payload).ok_or(ColdError::CorruptPayload)?;
                out.push((v, row, vals));
            }
        }
        Ok(out)
    }
}
