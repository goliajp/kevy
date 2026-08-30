//! Row segments — the persistent second backing behind `Value::Cold`.
//! A windowed table's out-of-window hash rows phase-change in place:
//! the key, the Entry and its TTL stay hot, the value becomes a stub
//! whose backing is an immutable segment file instead of the per-boot
//! vlog. Reads resolve through the same stub seam as the value tier;
//! a write promotes-then-writes; a replaced stub strands the segment
//! record for compaction (no tombstones — reads never reach a
//! stranded record, the hot stub is the only path in).
//!
//! Row segments are persistent truth: `enable_seg_rows` loads the
//! manifest's registered segments (keyed by the monotone seq embedded
//! in the file name — the stub's stable identity across restarts),
//! the AOF's `SEGMENTED` frame re-establishes each row's stub at
//! replay (demote-or-insert), and segments nothing references after
//! replay are swept as orphans (a crash between sealing and the frame
//! leaves exactly that). The stub snapshot record and the rewrite
//! frame — the parts that stop snapshots/rewrites carrying cold row
//! data — are the next train; until then both still materialize.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::value::{COLD_TAG_HASH, ColdRef, Value};
use crate::{Store, key_heap_bytes_for, tier_codec};

/// One shard's row-segment directory: the open segments and their
/// live/dead record accounting (compaction's future trigger feed).
pub(crate) struct SegRows {
    dir: PathBuf,
    /// Open segments, keyed by their stable seq (the file-name number
    /// a stub's `seg_ix` refers to — Vec-of-pairs, segment counts are
    /// small and lookups linear).
    segs: Vec<(u32, SegSlot)>,
    seq: u32,
}

struct SegSlot {
    /// Arc so a [`crate::SnapshotView`] can pin the segment across the
    /// serializer thread, exactly like the vlog file pins.
    seg: Arc<kevy_seg::Seg>,
    file: String,
    live: u64,
    dead: u64,
}

impl SegRows {
    fn slot(&self, seq: u32) -> &SegSlot {
        &self.segs.iter().find(|(q, _)| *q == seq).expect("stub names a loaded segment").1
    }

    fn slot_mut(&mut self, seq: u32) -> Option<&mut SegSlot> {
        self.segs.iter_mut().find(|(q, _)| *q == seq).map(|(_, s)| s)
    }
}

/// One sealed eviction batch: the segment's identity and EXACTLY the
/// keys it holds (the commit's phase-change list).
pub struct SealedRows {
    /// The segment's stable seq.
    pub seq: u32,
    /// Its file name — what the SEGMENTED frame carries.
    pub file: String,
    keys: Vec<Vec<u8>>,
}

/// The manifest meta tag row segments register under.
const ROW_TAG: &[u8] = b"rowcold:";

impl ColdRef {
    /// A stub pointing into the row-segment directory.
    pub(crate) fn seg(seq: u32, weight: u32, _type_tag: u8) -> Self {
        ColdRef::from_seg_parts(seq, weight)
    }

    /// Whether this stub's backing is a row segment (vs the vlog).
    pub(crate) fn is_seg(self) -> bool {
        self.seg_parts().is_some()
    }

    /// The segment's stable seq (seg backing only).
    pub(crate) fn seg_ix(self) -> u32 {
        self.offset as u32
    }
}

impl SegRows {
    fn read(&self, cref: ColdRef, key: &[u8]) -> Value {
        let slot = self.slot(cref.seg_ix());
        let payload = slot
            .seg
            .get(key)
            .expect("segrows: segment read failed — refused, not healed")
            .unwrap_or_else(|| {
                panic!(
                    "segrows: stub for {:?} points at segment '{}' (seq {}) which does not hold it",
                    String::from_utf8_lossy(key),
                    slot.file,
                    cref.seg_ix(),
                )
            });
        tier_codec::decode(cref.type_tag, payload)
            .expect("segrows: cold row decode failed — process bug")
    }
}

impl Store {
    /// Turn row segments on for this shard, rooted at `dir`, loading
    /// every manifest-registered row segment from the previous run —
    /// they are truth, and the AOF's SEGMENTED frames (or a stub
    /// snapshot) will reference them by seq. Idempotent.
    pub fn enable_seg_rows(&mut self, dir: &Path) -> Result<(), String> {
        if self.segrows.is_some() {
            return Ok(());
        }
        let mut segs = Vec::new();
        let mut seq = 0u32;
        if dir.exists() {
            let m = kevy_seg::Manifest::open(dir).map_err(|e| e.to_string())?;
            for e in m.live().filter(|e| e.meta.starts_with(ROW_TAG)) {
                let Some(q) = seq_of(&e.file) else {
                    return Err(format!("row segment '{}' has no parsable seq", e.file));
                };
                let seg = kevy_seg::Seg::open(&dir.join(&e.file))
                    .map_err(|err| format!("open {}: {err}", e.file))?;
                seq = seq.max(q + 1);
                segs.push((
                    q,
                    SegSlot { seg: Arc::new(seg), file: e.file.clone(), live: 0, dead: 0 },
                ));
            }
        }
        // The gate opens only when cold values can actually exist:
        // enabling the DIRECTORY costs nothing on the funnels; loaded
        // segments (or the first sealed one, or a loaded stub) do.
        self.cold_backing |= !segs.is_empty();
        self.segrows = Some(SegRows { dir: dir.to_path_buf(), segs, seq });
        Ok(())
    }

    /// After replay: rebuild each loaded segment's live count from the
    /// stubs that actually reference it, and sweep the segments nothing
    /// references — a crash between sealing and the SEGMENTED frame
    /// leaves exactly such an orphan (its rows replayed hot).
    pub fn sweep_orphan_row_segs(&mut self) {
        let Some(sr) = &mut self.segrows else { return };
        for (_, slot) in &mut sr.segs {
            slot.live = 0;
        }
        for (_, e) in &self.map {
            if let Value::Cold(c) = &e.value
                && c.is_seg()
                && let Some(slot) = sr.slot_mut(c.seg_ix())
            {
                slot.live += 1;
            }
        }
        let mut m = match kevy_seg::Manifest::open(&sr.dir) {
            Ok(m) => m,
            Err(_) => return,
        };
        sr.segs.retain(|(_, slot)| {
            if slot.live > 0 {
                return true;
            }
            let _ = m.drop_seg(&slot.file);
            let _ = std::fs::remove_file(sr.dir.join(&slot.file));
            false
        });
        // Files the ledger never learned about (a crash mid-build)
        // are plain garbage — the manifest sweep reclaims them.
        let _ = m.sweep(&sr.dir);
    }

    /// The two-phase producer face: seal the batch (durable half) and
    /// return `(seq, file)` for the caller to log a SEGMENTED frame
    /// BEFORE [`Store::commit_row_eviction`] phase-changes the rows —
    /// the R2c ordering (frame after the durable copy, before the hot
    /// deletion) that makes every crash gap recoverable.
    pub fn seal_rows_to_seg(
        &mut self,
        table: &[u8],
        keys: &[Vec<u8>],
    ) -> Result<Option<SealedRows>, String> {
        if self.segrows.is_none() {
            return Ok(None);
        }
        let mut rows: Vec<(&[u8], Vec<u8>)> = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(payload) = self.encode_evictable_row(key) {
                rows.push((key.as_slice(), payload));
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        rows.sort_by(|a, b| a.0.cmp(b.0));
        let seq = self.build_row_segment(table, &rows)?;
        let file = self.segrows.as_ref().expect("enabled above").slot(seq).file.clone();
        // Only the keys actually sealed may phase-change — a filtered
        // row (TTL-bearing, revived mid-batch, non-hash) stubbed at a
        // segment that does not hold it would be a ghost.
        let sealed = rows.iter().map(|(k, _)| k.to_vec()).collect();
        Ok(Some(SealedRows { seq, file, keys: sealed }))
    }

    /// Phase-change the sealed batch after its SEGMENTED frame is
    /// logged.
    pub fn commit_row_eviction(&mut self, sealed: &SealedRows) -> u64 {
        let mut n = 0u64;
        for key in &sealed.keys {
            if self.demote_row_to_seg(key, sealed.seq) {
                n += 1;
            }
        }
        if let Some(sr) = self.segrows.as_mut()
            && let Some(slot) = sr.slot_mut(sealed.seq)
        {
            slot.live += n;
        }
        n
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
            Value::Hash(_) | Value::SmallHashInline(_) | Value::PackedRow(_) => {
                tier_codec::encode(&e.value).map(|(payload, _tag)| payload)
            }
            _ => None,
        }
    }

    /// Seal `rows` (key-ascending) into a manifest-registered segment
    /// file and open it into the directory. The map is untouched.
    fn build_row_segment(
        &mut self,
        table: &[u8],
        rows: &[(&[u8], Vec<u8>)],
    ) -> Result<u32, String> {
        let sr = self.segrows.as_mut().expect("checked by caller");
        std::fs::create_dir_all(&sr.dir).map_err(|e| e.to_string())?;
        let seq = sr.seq;
        let file = format!("row-{}-{}.seg", hex_stem(table), seq);
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
        sr.segs.push((seq, SegSlot { seg: Arc::new(seg), file, live: 0, dead: 0 }));
        self.cold_backing = true;
        Ok(seq)
    }

    /// Phase-change one row to a seg-backed stub: the demote_in_place
    /// twin without the vlog append (the value is already sealed in
    /// the segment). Preserves TTL/LRU (both None/irrelevant here by
    /// the eviction filter), fires no events, clears no field TTLs.
    pub(crate) fn demote_row_to_seg(&mut self, key: &[u8], seg_ix: u32) -> bool {
        let Some(e) = self.map.get_mut(key) else { return false };
        if !matches!(e.value, Value::Hash(_) | Value::SmallHashInline(_) | Value::PackedRow(_)) {
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

    /// The loaded seq for a manifest-registered row-segment file name.
    pub(crate) fn row_seg_seq(&self, file: &str) -> Option<u32> {
        let sr = self.segrows.as_ref()?;
        sr.segs.iter().find(|(_, s)| s.file == file).map(|(q, _)| *q)
    }

    /// Every `(key, payload)` in segment `seq` — the replay stitch's
    /// walk. Collected owned: the caller mutates the map while
    /// stitching.
    pub(crate) fn row_seg_records(&self, seq: u32) -> Vec<(Vec<u8>, Vec<u8>)> {
        let sr = self.segrows.as_ref().expect("stitch ⇒ enabled");
        let slot = sr.slot(seq);
        let (lo, hi) = (slot.seg.meta().min_key.clone(), slot.seg.meta().max_key.clone());
        slot.seg
            .range(&lo, &hi)
            .map(|r| r.expect("segrows: segment read failed — refused, not healed"))
            .collect()
    }

    /// Insert a stub entry for a row the log never rebuilt hot (a
    /// rewritten log carries no cold-row commands). No events, no TTL
    /// (cold rows are TTL-free by the eviction filter).
    pub(crate) fn insert_row_stub(&mut self, key: &[u8], seq: u32, value_weight: u64) {
        let stub = ColdRef::seg(seq, value_weight.min(u64::from(u32::MAX)) as u32, COLD_TAG_HASH);
        let key_heap = key_heap_bytes_for(key);
        let mut e = crate::Entry::new(Value::Cold(stub), None);
        e.set_weight(key_heap);
        crate::apply_delta(&mut self.used_memory, key_heap as i64);
        self.map.insert(crate::SmallBytes::from_slice(key), e);
    }

    /// Load one snapshot stub record: the row's identity re-enters the
    /// map as a seg-backed stub (the segment directory, loaded before
    /// the snapshot, holds its data). TTL-free by the eviction filter.
    pub fn load_row_stub(&mut self, key: Vec<u8>, seq: u32, value_weight: u32) {
        self.cold_backing = true;
        self.insert_row_stub(&key, seq, u64::from(value_weight));
    }

    /// Fold a replay stitch's count into the segment's live tally.
    pub(crate) fn note_stitched(&mut self, seq: u32, n: u64) {
        if let Some(sr) = self.segrows.as_mut()
            && let Some(slot) = sr.slot_mut(seq)
        {
            slot.live += n;
        }
    }

    /// Decode a seg-backed stub's row. The panic doctrine matches the
    /// vlog's: a stub pointing at a missing/corrupt record is a
    /// process bug, surfaced loudly.
    pub(crate) fn segrow_read(&self, cref: ColdRef, key: &[u8]) -> Value {
        self.segrows.as_ref().expect("seg-backed stub ⇒ segrows enabled").read(cref, key)
    }

    /// A seg-backed stub died (DEL / expiry / promote / FLUSH): the
    /// segment record is now stranded — count it for compaction.
    pub(crate) fn segrow_note_dead(&mut self, cref: ColdRef) {
        if let Some(sr) = &mut self.segrows
            && let Some(slot) = sr.slot_mut(cref.seg_ix())
        {
            slot.dead += 1;
            slot.live = slot.live.saturating_sub(1);
        }
    }

    /// The live row segments' `(seq, file)` identities — the rewrite's
    /// trailing SEGMENTED frames name these.
    pub fn row_seg_files(&self) -> Vec<(u32, String)> {
        self.segrows
            .as_ref()
            .map(|sr| sr.segs.iter().map(|(q, s)| (*q, s.file.clone())).collect())
            .unwrap_or_default()
    }

    /// The open segment handles, for a snapshot view's pins.
    pub(crate) fn segrow_pins(&self) -> Vec<(u32, Arc<kevy_seg::Seg>)> {
        self.segrows
            .as_ref()
            .map(|sr| sr.segs.iter().map(|(q, s)| (*q, s.seg.clone())).collect())
            .unwrap_or_default()
    }

    /// FLUSHALL/FLUSHDB: every stub died with the map; the segments
    /// are all garbage. Unregister and unlink now (the ledger never
    /// points at nothing, and nothing points at the ledger).
    pub(crate) fn segrows_flush(&mut self) {
        let Some(sr) = &mut self.segrows else { return };
        if let Ok(mut m) = kevy_seg::Manifest::open(&sr.dir) {
            let stale: Vec<String> =
                m.live().filter(|e| e.meta.starts_with(ROW_TAG)).map(|e| e.file.clone()).collect();
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

/// The stable seq embedded in a row-segment file name
/// (`row-<hex>-<seq>.seg`).
fn seq_of(file: &str) -> Option<u32> {
    file.strip_suffix(".seg")?.rsplit('-').next()?.parse().ok()
}
