//! Row segments — the persistent second backing behind `Value::Cold`.
//! A windowed table's out-of-window hash rows phase-change in place:
//! the key, the Entry and its TTL stay hot, the value becomes a stub
//! whose backing is an immutable segment file instead of the per-boot
//! vlog. Reads resolve through the same stub seam as the value tier;
//! a write promotes-then-writes; a replaced stub strands the segment
//! record for compaction (no tombstones — reads never reach a
//! stranded record, the hot stub is the only path in).
//!
//! In this train row segments are derived spill: a restart replays
//! rows hot and re-slides (the stale segments unregister on enable).
//! The persistent-truth half (stub snapshot records, the SEGMENTED
//! frame producer, the crash matrix) is the next train.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::value::{COLD_TAG_HASH, ColdRef, Value};
use crate::{Store, key_heap_bytes_for, tier_codec};

/// One shard's row-segment directory: the open segments and their
/// live/dead record accounting (compaction's future trigger feed).
pub(crate) struct SegRows {
    dir: PathBuf,
    segs: Vec<SegSlot>,
    seq: u64,
}

struct SegSlot {
    /// Arc so a [`crate::SnapshotView`] can pin the segment across the
    /// serializer thread, exactly like the vlog file pins.
    seg: Arc<kevy_seg::Seg>,
    live: u64,
    dead: u64,
}

/// The manifest meta tag row segments register under.
const ROW_TAG: &[u8] = b"rowcold:";

/// High bit of `ColdRef::file_id`: the stub's backing store is a row
/// SEGMENT (persistent, keyed by the row key) rather than the per-boot
/// vlog. Segment stubs reuse the same 24-byte shape — `offset` holds
/// the segment-directory index, `len` is unused — so every match site
/// that answers stage-1 questions from the stub works unchanged.
pub(crate) const SEG_BACKING: u32 = 1 << 31;

impl ColdRef {
    /// A stub pointing into the row-segment directory.
    pub(crate) fn seg(seg_ix: u32, weight: u32, type_tag: u8) -> Self {
        ColdRef {
            offset: u64::from(seg_ix),
            file_id: SEG_BACKING,
            len: 0,
            weight,
            type_tag,
            touched: 0,
        }
    }

    /// Whether this stub's backing is a row segment (vs the vlog).
    pub(crate) fn is_seg(self) -> bool {
        self.file_id & SEG_BACKING != 0
    }

    /// The segment-directory index (seg backing only).
    pub(crate) fn seg_ix(self) -> u32 {
        self.offset as u32
    }
}

impl SegRows {
    fn read(&self, cref: ColdRef, key: &[u8]) -> Value {
        let slot = &self.segs[cref.seg_ix() as usize];
        let payload = slot
            .seg
            .get(key)
            .expect("segrows: segment read failed — refused, not healed")
            .expect("segrows: stub points at a record the segment does not hold");
        tier_codec::decode(cref.type_tag, payload)
            .expect("segrows: cold row decode failed — process bug")
    }
}

impl Store {
    /// Turn row segments on for this shard, rooted at `dir`. Unregisters
    /// any previous run's row segments first (derived spill: replay
    /// brought every row back hot, so the old segments are unreachable
    /// and their names must not collide with this run's).
    pub fn enable_seg_rows(&mut self, dir: &Path) -> Result<(), String> {
        if self.segrows.is_some() {
            return Ok(());
        }
        if dir.exists() {
            let mut m = kevy_seg::Manifest::open(dir).map_err(|e| e.to_string())?;
            let stale: Vec<String> = m
                .live()
                .filter(|e| e.meta.starts_with(ROW_TAG))
                .map(|e| e.file.clone())
                .collect();
            for f in stale {
                m.drop_seg(&f).map_err(|e| e.to_string())?;
                let _ = std::fs::remove_file(dir.join(&f));
            }
        }
        self.segrows = Some(SegRows { dir: dir.to_path_buf(), segs: Vec::new(), seq: 0 });
        Ok(())
    }

    /// Evict `keys` (a windowed table's out-of-window hash rows) into
    /// one sealed segment and phase-change each into a seg-backed stub.
    /// Skips — never fails on — rows that are absent, non-hash, already
    /// cold, or TTL-bearing (key or field TTLs keep a row hot: short-
    /// lived data has no business on disk). Returns how many evicted.
    /// Build-then-demote: an I/O failure leaves every row hot.
    pub fn evict_rows_to_seg(&mut self, table: &[u8], keys: &[Vec<u8>]) -> Result<u64, String> {
        if self.segrows.is_none() {
            return Ok(0);
        }
        let mut rows: Vec<(&[u8], Vec<u8>)> = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(payload) = self.encode_evictable_row(key) {
                rows.push((key.as_slice(), payload));
            }
        }
        if rows.is_empty() {
            return Ok(0);
        }
        rows.sort_by(|a, b| a.0.cmp(b.0));
        let seg_ix = self.build_row_segment(table, &rows)?;
        let mut n = 0u64;
        for (key, _) in &rows {
            if self.demote_row_to_seg(key, seg_ix) {
                n += 1;
            }
        }
        self.segrows.as_mut().expect("enabled above").segs[seg_ix as usize].live = n;
        Ok(n)
    }

    /// The row's encoded payload iff it is evictable right now.
    fn encode_evictable_row(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        if self.hfttl.get(key).is_some_and(|m| !m.is_empty()) {
            return None;
        }
        let e = self.live_entry(key)?;
        if e.expire_at_ns.is_some() {
            return None;
        }
        match &e.value {
            Value::Hash(_) | Value::SmallHashInline(_) => {
                tier_codec::encode(&e.value).map(|(payload, _tag)| payload)
            }
            _ => None,
        }
    }

    /// Seal `rows` (key-ascending) into a manifest-registered segment
    /// file and open it into the directory. The map is untouched.
    fn build_row_segment(&mut self, table: &[u8], rows: &[(&[u8], Vec<u8>)]) -> Result<u32, String> {
        let sr = self.segrows.as_mut().expect("checked by caller");
        std::fs::create_dir_all(&sr.dir).map_err(|e| e.to_string())?;
        let file = format!("row-{}-{}.seg", hex_stem(table), sr.seq);
        sr.seq += 1;
        let path = sr.dir.join(&file);
        let build = || -> Result<kevy_seg::SegMeta, String> {
            let mut b = kevy_seg::SegBuilder::create(&path).map_err(|e| e.to_string())?;
            for (k, payload) in rows {
                b.push(k, payload).map_err(|e| e.to_string())?;
            }
            b.finish().map_err(|e| e.to_string())
        };
        let meta = build().inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;
        let mut m = kevy_seg::Manifest::open(&sr.dir).map_err(|e| e.to_string())?;
        m.add(kevy_seg::ManifestEntry {
            file: file.clone(),
            meta: [ROW_TAG, table].concat(),
            min_key: meta.min_key,
            max_key: meta.max_key,
            records: meta.records,
        })
        .map_err(|e| e.to_string())?;
        let seg = kevy_seg::Seg::open(&path).map_err(|e| format!("reopen {file}: {e}"))?;
        sr.segs.push(SegSlot { seg: Arc::new(seg), live: 0, dead: 0 });
        Ok((sr.segs.len() - 1) as u32)
    }

    /// Phase-change one row to a seg-backed stub: the demote_in_place
    /// twin without the vlog append (the value is already sealed in
    /// the segment). Preserves TTL/LRU (both None/irrelevant here by
    /// the eviction filter), fires no events, clears no field TTLs.
    pub(crate) fn demote_row_to_seg(&mut self, key: &[u8], seg_ix: u32) -> bool {
        let Some(e) = self.map.get_mut(key) else { return false };
        if !matches!(e.value, Value::Hash(_) | Value::SmallHashInline(_)) {
            return false;
        }
        let key_heap = key_heap_bytes_for(key);
        let old_w = e.weight();
        let value_w = old_w.saturating_sub(key_heap);
        let stub = ColdRef::seg(seg_ix, value_w.min(u64::from(u32::MAX)) as u32, COLD_TAG_HASH);
        let old_value = core::mem::replace(&mut e.value, Value::Cold(stub));
        e.set_weight(key_heap);
        crate::apply_delta(&mut self.used_memory, -(value_w as i64));
        self.maybe_offload_drop(old_value);
        true
    }

    /// Decode a seg-backed stub's row. The panic doctrine matches the
    /// vlog's: a stub pointing at a missing/corrupt record is a
    /// process bug, surfaced loudly.
    pub(crate) fn segrow_read(&self, cref: ColdRef, key: &[u8]) -> Value {
        self.segrows
            .as_ref()
            .expect("seg-backed stub ⇒ segrows enabled")
            .read(cref, key)
    }

    /// A seg-backed stub died (DEL / expiry / promote / FLUSH): the
    /// segment record is now stranded — count it for compaction.
    pub(crate) fn segrow_note_dead(&mut self, cref: ColdRef) {
        if let Some(sr) = &mut self.segrows {
            let slot = &mut sr.segs[cref.seg_ix() as usize];
            slot.dead += 1;
            slot.live = slot.live.saturating_sub(1);
        }
    }

    /// The open segment handles, for a snapshot view's pins.
    pub(crate) fn segrow_pins(&self) -> Vec<Arc<kevy_seg::Seg>> {
        self.segrows
            .as_ref()
            .map(|sr| sr.segs.iter().map(|s| s.seg.clone()).collect())
            .unwrap_or_default()
    }

    /// FLUSHALL/FLUSHDB: every stub died with the map; the segments
    /// are all garbage. Unregister and unlink now (the ledger never
    /// points at nothing, and nothing points at the ledger).
    pub(crate) fn segrows_flush(&mut self) {
        let Some(sr) = &mut self.segrows else { return };
        if let Ok(mut m) = kevy_seg::Manifest::open(&sr.dir) {
            let stale: Vec<String> = m
                .live()
                .filter(|e| e.meta.starts_with(ROW_TAG))
                .map(|e| e.file.clone())
                .collect();
            for f in stale {
                let _ = m.drop_seg(&f);
                let _ = std::fs::remove_file(sr.dir.join(&f));
            }
        }
        sr.segs.clear();
    }
}

/// Table names are free bytes; the file name needs a safe stem.
fn hex_stem(name: &[u8]) -> String {
    name.iter().map(|b| format!("{b:02x}")).collect()
}
