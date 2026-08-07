//! The sliding-window runtime for scalar indexes — shared by the
//! server and the embedded store (one implementation, so the two
//! faces cannot drift): boundary maintenance, the eviction slide,
//! and the cold half of range/count.
//!
//! Cold segments are derived spill, not truth (the rows stay hot; the
//! index is rebuilt from them on boot) — so a failed slide simply
//! leaves the tree untouched (the batch is read before it is cut),
//! and a restart drops the segment set and re-slides.

use std::collections::HashMap;
use std::path::Path;

#[path = "text.rs"]
mod text;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
pub use text::{ColdHit, ColdPage, ColdPageQuery, TextColdDir};

use kevy_index::{
    ColdBloom, ColdEntryRow, FacetBucket, IndexValue, ScalarClauses, ScalarHit, ValType,
    WindowAudit, WindowShape, WindowSpec, claused_over, decode_seg_key, decode_seg_values,
    encode_seg_values,
    seg_bounds, seg_key, values_pass, window_bound, window_value_of,
};

/// One index's window state on one shard.
pub struct WindowRt {
    pub spec: WindowSpec,
    /// Which tree shape the boundary lives in — a plain i64 index or
    /// a composite the window column leads (see [`WindowShape`]).
    pub shape: WindowShape,
    /// Current boundary (bucket-aligned): entries with value < w are
    /// cold. `i64::MIN` = nothing evicted yet.
    w: i64,
    /// Segment file name counter.
    seq: u64,
    /// Sealed segments with the sequence number each was built under —
    /// the number a tombstone is compared against.
    cold: Vec<(u64, kevy_seg::Seg)>,
    /// Rows that MAY have cold entries — consulted before spending a
    /// tombstone on a write.
    bloom: ColdBloom,
    /// Rows whose cold entries are shadowed, each recorded with the
    /// sequence number the shadow reaches: entries in segments sealed
    /// BEFORE it are hidden, entries sealed after it are not.
    ///
    /// A flat set was wrong and lost rows for it. The set is fed by a
    /// bloom, so a write can tombstone a row that has no cold entry at
    /// all; when that row later slid, the stale shadow hid the live
    /// entry it had just been given, permanently. Recording how far
    /// the shadow reaches costs one `u64` and makes it exact — the
    /// same property `text.rs` states for its own tombstones.
    ///
    /// A row earns one by being rewritten, deleted, or revived after
    /// eviction. Memory-only: replayed writes re-earn them through the
    /// same bloom on the rebuilt state.
    tombs: HashMap<Vec<u8>, u64>,
    /// Ticks that cost exactly one comparison (the idle-convergence
    /// gate counter).
    pub idle_ticks: u64,
    /// Whether this boot's stale derived segments (a previous run's
    /// spill for this index) were dropped yet. Done lazily on the
    /// first slide: they are unreachable (the boundary restarts at
    /// MIN) and their manifest entries would collide with this run's
    /// file names.
    cleaned: bool,
}

impl WindowRt {
    pub fn new(spec: WindowSpec, shape: WindowShape) -> Self {
        Self {
            spec,
            shape,
            w: i64::MIN,
            seq: 0,
            cold: Vec::new(),
            bloom: ColdBloom::new(4096),
            tombs: HashMap::new(),
            idle_ticks: 0,
            cleaned: false,
        }
    }

    pub fn has_cold(&self) -> bool {
        !self.cold.is_empty()
    }

    /// The current eviction boundary: entries with window value below
    /// this are cold. `i64::MIN` = nothing has evicted yet. Read by
    /// the window-narrowing observation (a query's `lower - boundary`
    /// margin), never interpreted beyond ordering.
    pub fn boundary(&self) -> i64 {
        self.w
    }

    /// Is this row's entry in the segment sealed as `seq` shadowed?
    /// A shadow reaches only backwards: it was recorded to hide what
    /// existed when the row changed, and cannot hide what the row was
    /// given afterwards.
    fn shadowed(&self, row: &[u8], seq: u64) -> bool {
        self.tombs.get(row).is_some_and(|&reach| seq < reach)
    }

    /// The write path saw `row_key` change: shadow whatever cold entry
    /// it may have RIGHT NOW. A bloom false positive spends one stray
    /// map entry that shadows nothing, which is the point: the reach
    /// is the current sequence, and anything this row is given later
    /// is sealed above it.
    pub fn on_row_write(&mut self, row_key: &[u8]) {
        if self.bloom.contains(row_key) {
            self.tombs.insert(row_key.to_vec(), self.seq);
        }
    }

    /// What an audit needs from the cold side: the boundary, the tree
    /// shape, and how many entries are actually down there. `None`
    /// until something has slid.
    ///
    /// The count is over each segment's OWN extent rather than a value
    /// range, because the caller wants "everything cold" and building
    /// an unbounded upper bound differs per tree shape — a segment
    /// already knows its own first and last key.
    pub fn audit(&self, ty: ValType) -> Option<WindowAudit> {
        if self.w == i64::MIN {
            return None;
        }
        let mut cold_live = 0u64;
        for (seq, seg) in &self.cold {
            let (lo, hi) = (seg.meta().min_key.clone(), seg.meta().max_key.clone());
            if self.tombs.is_empty() {
                cold_live += seg.count_range(&lo, &hi).ok()?;
                continue;
            }
            // Tombstones are bloom-gated, so a stray one can name a row
            // with no cold entry at all. Counting records minus tombs
            // would under-report and the audit would invent a hole, so
            // the live entries are counted directly.
            for r in seg.range(&lo, &hi) {
                let (k, _) = r.ok()?;
                let Some((_, row)) = decode_seg_key(ty, &k) else { continue };
                if !self.shadowed(&row, *seq) {
                    cold_live += 1;
                }
            }
        }
        Some(WindowAudit { boundary: self.w, shape: self.shape, cold_live })
    }

    /// Cold count of values in `[min, max]`: fast whole-segment
    /// arithmetic while no tombstones exist (the common state), a
    /// decode walk once any do. `Err` = a segment refused (corrupt
    /// derived spill) — the query reports it, never a partial number.
    pub fn cold_count(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
    ) -> Result<u64, String> {
        let (lo, hi) = seg_bounds(min, max);
        if self.tombs.is_empty() {
            let mut n = 0u64;
            for (_, s) in &self.cold {
                n += s.count_range(&lo, &hi).map_err(|e| e.to_string())?;
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
    pub fn cold_hits(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&kevy_index::Cursor>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, IndexValue)>, String> {
        let (lo, hi) = seg_bounds(min, max);
        let mut out = Vec::new();
        for (seq, seg) in &self.cold {
            for r in seg.range(&lo, &hi) {
                let (k, _) = r.map_err(|e| e.to_string())?;
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
    pub fn cold_claused_count(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
        filters: &[(usize, kevy_index::ValueTest)],
    ) -> Result<u64, String> {
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
    pub fn cold_claused(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&kevy_index::Cursor>,
        c: &ScalarClauses<'_>,
    ) -> Result<(Vec<ScalarHit>, Vec<Vec<FacetBucket>>), String> {
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
    ) -> Result<Vec<ColdEntryRow>, String> {
        let (lo, hi) = seg_bounds(min, max);
        let mut out = Vec::new();
        for (seq, seg) in &self.cold {
            for r in seg.range(&lo, &hi) {
                let (k, payload) = r.map_err(|e| e.to_string())?;
                let (v, row) =
                    decode_seg_key(ty, &k).ok_or_else(|| "corrupt cold key".to_string())?;
                if self.shadowed(&row, *seq) {
                    continue;
                }
                if cursor.is_some_and(|c| (&v, row.as_slice()) <= (&c.value, c.key.as_slice())) {
                    continue;
                }
                let vals = decode_seg_values(&payload)
                    .ok_or_else(|| "corrupt cold payload".to_string())?;
                out.push((v, row, vals));
            }
        }
        Ok(out)
    }

    /// The row keys that would evict if the boundary advanced now —
    /// the row-eviction half reads this BEFORE [`Self::slide`] cuts
    /// the index, so a failed row eviction leaves both layers hot and
    /// the next tick retries the whole batch. No state changes.
    pub fn pending_rows(&self, seg: &kevy_index::Segment) -> Option<Vec<Vec<u8>>> {
        let max = window_value_of(seg.max_value()?, self.shape)?;
        let target = bucket_floor(max.saturating_sub(self.spec.span), self.spec.bucket);
        if target <= self.w {
            return None;
        }
        let bound = window_bound(target, self.shape);
        let rows: Vec<Vec<u8>> = seg.iter_below(&bound).map(|(_, k)| k.to_vec()).collect();
        (!rows.is_empty()).then_some(rows)
    }

    /// Advance the boundary and evict the out-of-window tree prefix
    /// into a segment. One comparison when there is nothing to do.
    /// Build-then-cut: an I/O failure leaves the tree untouched and
    /// the boundary unmoved — the next tick retries.
    pub fn slide(
        &mut self,
        index_name: &[u8],
        seg: &mut kevy_index::Segment,
        segs_dir: &Path,
    ) -> Result<bool, String> {
        let Some(max) = seg.max_value().and_then(|v| window_value_of(v, self.shape)) else {
            self.idle_ticks += 1;
            return Ok(false);
        };
        let target = bucket_floor(max.saturating_sub(self.spec.span), self.spec.bucket);
        if target <= self.w {
            self.idle_ticks += 1;
            return Ok(false);
        }
        let bound = window_bound(target, self.shape);
        if seg.iter_below(&bound).next().is_none() {
            self.w = target;
            return Ok(false);
        }
        if !self.cleaned {
            clean_stale_derived(index_name, segs_dir)?;
            self.cleaned = true;
        }
        let file = self.build_segment(index_name, seg, &bound, segs_dir)?;
        let batch = seg.split_off_below(&bound);
        for (_, k) in &batch {
            self.bloom.insert(k);
        }
        // `seq` was consumed by `build_segment`, so this file's own
        // number is one below the counter it left behind.
        self.cold.push((
            self.seq - 1,
            kevy_seg::Seg::open(&segs_dir.join(&file)).map_err(|e| format!("reopen {file}: {e}"))?,
        ));
        self.probe(index_name, batch.len());
        self.w = target;
        Ok(true)
    }

    /// `KEVY_PROBE_SLIDE=1`: one line per slide with what was sealed,
    /// what left the tree, and how many shadows are outstanding.
    ///
    /// This is the instrument that found the stale-tombstone loss. The
    /// first three numbers refute the obvious theory (the seal drops
    /// what arrives mid-build — it does not; sealed always equals
    /// split_off), which is what left the tombstone count as the only
    /// remaining place the missing rows could be.
    fn probe(&self, index_name: &[u8], split_off: usize) {
        if std::env::var_os("KEVY_PROBE_SLIDE").is_none() {
            return;
        }
        let sealed = self.cold.last().map(|c| c.1.meta().records).unwrap_or(0);
        eprintln!(
            "PROBE slide {} sealed={sealed} split_off={split_off} tombs={} {}",
            String::from_utf8_lossy(index_name),
            self.tombs.len(),
            if sealed as usize == split_off { "ok" } else { "MISMATCH" }
        );
    }

    /// Seal the below-bound prefix into a manifest-registered segment
    /// file; the tree is not touched.
    fn build_segment(
        &mut self,
        index_name: &[u8],
        seg: &kevy_index::Segment,
        bound: &IndexValue,
        segs_dir: &Path,
    ) -> Result<String, String> {
        std::fs::create_dir_all(segs_dir).map_err(|e| e.to_string())?;
        let file = format!("idx-{}-{}.seg", hex_stem(index_name), self.seq);
        self.seq += 1;
        let path = segs_dir.join(&file);
        let build = || -> Result<kevy_seg::SegMeta, String> {
            let mut b = kevy_seg::SegBuilder::create(&path).map_err(|e| e.to_string())?;
            for (v, k) in seg.iter_below(bound) {
                // The payload carries the row's stored VALUES so the
                // clause-carrying cold path never re-reads the row
                // (which may itself have gone cold). No declared
                // values = the empty payload, the a-train shape.
                let vals = seg.stored_row(k);
                b.push(&seg_key(v, k), &encode_seg_values(&vals)).map_err(|e| e.to_string())?;
            }
            b.finish().map_err(|e| e.to_string())
        };
        let meta = build().inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;
        let mut m = kevy_seg::Manifest::open(segs_dir).map_err(|e| e.to_string())?;
        m.add(kevy_seg::ManifestEntry {
            file: file.clone(),
            meta: [b"idxcold:", index_name].concat(),
            min_key: meta.min_key,
            max_key: meta.max_key,
            records: meta.records,
        })
        .map_err(|e| e.to_string())?;
        Ok(file)
    }
}

/// Drop a previous run's derived segments for `index_name`: their
/// manifest entries unregister first, then the files unlink (the
/// ledger never points at nothing).
fn clean_stale_derived(index_name: &[u8], segs_dir: &Path) -> Result<(), String> {
    if !segs_dir.exists() {
        return Ok(());
    }
    let mut m = kevy_seg::Manifest::open(segs_dir).map_err(|e| e.to_string())?;
    let tag = [b"idxcold:", index_name].concat();
    let stale: Vec<String> =
        m.live().filter(|e| e.meta == tag).map(|e| e.file.clone()).collect();
    for f in stale {
        m.drop_seg(&f).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(segs_dir.join(&f));
    }
    Ok(())
}

/// The window boundary advances in whole buckets (floor).
fn bucket_floor(v: i64, bucket: i64) -> i64 {
    v - v.rem_euclid(bucket)
}

/// Index names are free bytes; the segment file name needs a safe
/// stem. Hex is unambiguous and the manifest carries the real name.
fn hex_stem(name: &[u8]) -> String {
    name.iter().map(|b| format!("{b:02x}")).collect()
}
