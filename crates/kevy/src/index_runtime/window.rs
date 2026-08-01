//! The sliding-window runtime for scalar indexes: boundary
//! maintenance, the eviction slide, and the cold half of range/count.
//!
//! Cold segments are derived spill, not truth (the rows stay hot; the
//! index is rebuilt from them on boot) — so a failed slide simply
//! leaves the tree untouched (the batch is read before it is cut),
//! and a restart drops the segment set and re-slides.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use kevy_index::{ColdBloom, IndexValue, ValType, WindowSpec, decode_seg_key, seg_bounds, seg_key};

/// One index's window state on one shard.
pub(crate) struct WindowRt {
    pub(crate) spec: WindowSpec,
    /// Current boundary (bucket-aligned): entries with value < w are
    /// cold. `i64::MIN` = nothing evicted yet.
    w: i64,
    /// Segment file name counter.
    seq: u64,
    cold: Vec<kevy_seg::Seg>,
    /// Rows that MAY have cold entries — consulted before spending a
    /// tombstone on a write.
    bloom: ColdBloom,
    /// Rows whose cold entries are shadowed (rewritten / deleted /
    /// revived after eviction). Memory-only: replayed writes re-earn
    /// them through the same bloom on the rebuilt state.
    tombs: HashSet<Vec<u8>>,
    /// Ticks that cost exactly one comparison (the idle-convergence
    /// gate counter).
    pub(crate) idle_ticks: u64,
    /// Whether this boot's stale derived segments (a previous run's
    /// spill for this index) were dropped yet. Done lazily on the
    /// first slide: they are unreachable (the boundary restarts at
    /// MIN) and their manifest entries would collide with this run's
    /// file names.
    cleaned: bool,
}

impl WindowRt {
    pub(crate) fn new(spec: WindowSpec) -> Self {
        Self {
            spec,
            w: i64::MIN,
            seq: 0,
            cold: Vec::new(),
            bloom: ColdBloom::new(4096),
            tombs: HashSet::new(),
            idle_ticks: 0,
            cleaned: false,
        }
    }

    pub(crate) fn has_cold(&self) -> bool {
        !self.cold.is_empty()
    }

    /// The write path saw `row_key` change: shadow any cold entry it
    /// may have. A bloom false positive spends one stray set entry.
    pub(crate) fn on_row_write(&mut self, row_key: &[u8]) {
        if self.bloom.contains(row_key) {
            self.tombs.insert(row_key.to_vec());
        }
    }

    /// Cold count of values in `[min, max]`: fast whole-segment
    /// arithmetic while no tombstones exist (the common state), a
    /// decode walk once any do. `Err` = a segment refused (corrupt
    /// derived spill) — the query reports it, never a partial number.
    pub(crate) fn cold_count(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
    ) -> Result<u64, String> {
        let (lo, hi) = seg_bounds(min, max);
        if self.tombs.is_empty() {
            let mut n = 0u64;
            for s in &self.cold {
                n += s.count_range(&lo, &hi).map_err(|e| e.to_string())?;
            }
            return Ok(n);
        }
        Ok(self.cold_hits(ty, min, max, usize::MAX)?.len() as u64)
    }

    /// Cold hits of `[min, max]` in value order, tombstones skipped,
    /// at most `limit`. Segments hold disjoint ascending value ranges
    /// (each slide covers `[old_w, new_w)`), so chaining them in
    /// creation order IS value order. `Err` on a corrupt segment —
    /// never a silent partial page.
    pub(crate) fn cold_hits(
        &self,
        ty: ValType,
        min: &IndexValue,
        max: &IndexValue,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, IndexValue)>, String> {
        let (lo, hi) = seg_bounds(min, max);
        let mut out = Vec::new();
        for seg in &self.cold {
            for r in seg.range(&lo, &hi) {
                let (k, _) = r.map_err(|e| e.to_string())?;
                let Some((v, row)) = decode_seg_key(ty, &k) else { continue };
                if self.tombs.contains(&row) {
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

    /// Advance the boundary and evict the out-of-window tree prefix
    /// into a segment. One comparison when there is nothing to do.
    /// Build-then-cut: an I/O failure leaves the tree untouched and
    /// the boundary unmoved — the next tick retries.
    pub(crate) fn slide(
        &mut self,
        index_name: &[u8],
        seg: &mut kevy_index::Segment,
        segs_dir: &Path,
    ) -> Result<bool, String> {
        let Some(&IndexValue::I64(max)) = seg.max_value() else {
            self.idle_ticks += 1;
            return Ok(false);
        };
        let target = bucket_floor(max.saturating_sub(self.spec.span), self.spec.bucket);
        if target <= self.w {
            self.idle_ticks += 1;
            return Ok(false);
        }
        let bound = IndexValue::I64(target);
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
        self.cold.push(
            kevy_seg::Seg::open(&segs_dir.join(&file)).map_err(|e| format!("reopen {file}: {e}"))?,
        );
        self.w = target;
        Ok(true)
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
                b.push(&seg_key(v, k), &[]).map_err(|e| e.to_string())?;
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

/// The per-shard segment directory, when persistence is on. No data
/// dir = memory-only deployment = nothing slides (declaring a window
/// is allowed; it simply stays all-hot).
pub(crate) fn shard_segs_dir(state: &crate::state::RuntimeState, shard_id: usize) -> Option<PathBuf> {
    state.sidecar_dir().map(|d| kevy_persist::layout::segs_dir(d, shard_id))
}
