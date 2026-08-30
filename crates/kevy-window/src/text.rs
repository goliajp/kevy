//! The text index's cold half on one shard: the per-bucket frozen
//! segments a windowed table's out-of-window documents move into, the
//! staleness shadow (bloom + per-segment tombstones), and the
//! query-side contributions — corpus statistics for pass 1 and scored
//! hits for pass 2, both on the injected-stats scale so cold and hot
//! scores are comparable by construction.
//!
//! A tombstone is exact, not approximate: each frozen document also
//! carries a NUL-prefixed forward record (`\0` ++ row key — tokens
//! never start with NUL, so the namespaces cannot collide), and a
//! staling write reads it back to withdraw that document's n_docs,
//! total_len and per-term df from the segment's contribution. Pass-1
//! statistics therefore stay equal to a never-windowed control's
//! through rewrites, deletes and revivals. Shadows are per (row,
//! segment): a revived row that later re-freezes into a NEW segment
//! is live there while its stale entries stay dead.
//!
//! Cold text segments are derived spill (indexes rebuild on boot):
//! a restart drops the previous run's set and re-freezes.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use kevy_index::ColdBloom;
use kevy_text::TextSegment;
use kevy_text::cold::decode_fwd;

/// The manifest meta tag prefix cold text segments register under:
/// `txtcold:<index-name>:<n_docs>:<total_len>`.
const TXT_TAG: &[u8] = b"txtcold:";

/// One open cold segment with its LIVE corpus contribution — the
/// frozen numbers minus every tombstoned document's exact share.
pub(super) struct ColdSeg {
    pub(super) seg: kevy_seg::Seg,
    pub(super) seq: u32,
    pub(super) n_docs: u64,
    pub(super) total_len: u64,
}

#[path = "text_query.rs"]
mod query;
pub use query::{ColdHit, ColdPage, ColdPageQuery};

/// The frozen text segments for one windowed full-text index on one
/// shard, plus the bloom and tombstones that let a query skip or correct
/// them without opening a file.
pub struct TextColdDir {
    pub(super) segs: Vec<ColdSeg>,
    seq: u32,
    cleaned: bool,
    bloom: ColdBloom,
    /// row key → segment seqs whose frozen entries for it are dead.
    pub(super) tombs: HashMap<Vec<u8>, HashSet<u32>>,
    /// term → tombstoned document count, summed across segments; the
    /// pass-1 df correction (header df is freeze-time truth).
    pub(super) df_dead: HashMap<Vec<u8>, u32>,
}

impl Default for TextColdDir {
    fn default() -> Self {
        Self::new()
    }
}

impl TextColdDir {
    /// An empty directory: no segments, a fresh bloom, no tombstones.
    pub fn new() -> Self {
        Self {
            segs: Vec::new(),
            seq: 0,
            cleaned: false,
            bloom: ColdBloom::new(4096),
            tombs: HashMap::new(),
            df_dead: HashMap::new(),
        }
    }

    /// Whether any segment has been sealed. `false` lets a query stay
    /// entirely in the live index.
    pub fn has_cold(&self) -> bool {
        !self.segs.is_empty()
    }

    /// The write path saw this row change: shadow its frozen entries
    /// and withdraw its statistics, exactly, in every segment that
    /// holds it (its forward record says which, and what to subtract).
    pub fn on_row_write(&mut self, row_key: &[u8]) {
        if !self.bloom.contains(row_key) {
            return;
        }
        let mut fwd_key = vec![0u8];
        fwd_key.extend_from_slice(row_key);
        for cs in &mut self.segs {
            let shadowed = self.tombs.get(row_key).is_some_and(|s| s.contains(&cs.seq));
            if shadowed {
                continue;
            }
            let Ok(Some(payload)) = cs.seg.get(&fwd_key) else { continue };
            let Some(rec) = decode_fwd(&payload) else { continue };
            cs.n_docs = cs.n_docs.saturating_sub(1);
            cs.total_len = cs.total_len.saturating_sub(u64::from(rec.dl));
            for t in rec.terms {
                *self.df_dead.entry(t).or_insert(0) += 1;
            }
            self.tombs.entry(row_key.to_vec()).or_default().insert(cs.seq);
        }
    }

    /// Freeze `keys` out of the hot text segment into one sealed
    /// bucket segment. Failure leaves the hot segment SHRUNK but the
    /// batch unfrozen on disk — acceptable for derived spill (the
    /// entries are rebuildable from rows), reported to the caller.
    pub fn freeze_batch(
        &mut self,
        ts: &mut TextSegment,
        index_name: &[u8],
        keys: &[Vec<u8>],
        segs_dir: &Path,
    ) -> Result<bool, String> {
        if !self.cleaned {
            clean_stale(index_name, segs_dir)?;
            self.cleaned = true;
        }
        let Some(bucket) = ts.freeze_docs(keys) else { return Ok(false) };
        std::fs::create_dir_all(segs_dir).map_err(|e| e.to_string())?;
        let file = format!("txt-{}-{}.seg", hex_stem(index_name), self.seq);
        let seq = self.seq;
        self.seq += 1;
        let path = segs_dir.join(&file);
        write_seg_file(&path, &bucket).inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;
        let mut m = kevy_seg::Manifest::open(segs_dir).map_err(|e| e.to_string())?;
        let mut meta = TXT_TAG.to_vec();
        meta.extend_from_slice(index_name);
        meta.extend_from_slice(format!(":{}:{}", bucket.n_docs, bucket.total_len).as_bytes());
        m.add(kevy_seg::ManifestEntry {
            file: file.clone(),
            meta,
            min_key: bucket.fwd.keys().next().cloned().unwrap_or_default(),
            max_key: bucket.terms.keys().next_back().cloned().unwrap_or_default(),
            records: (bucket.fwd.len() + bucket.terms.len()) as u64,
        })
        .map_err(|e| e.to_string())?;
        let seg = kevy_seg::Seg::open(&path).map_err(|e| format!("reopen {file}: {e}"))?;
        self.segs.push(ColdSeg { seg, seq, n_docs: bucket.n_docs, total_len: bucket.total_len });
        for k in keys {
            self.bloom.insert(k);
        }
        Ok(true)
    }
}

/// Write one bucket to disk: forward records first (`\0`-prefixed row
/// keys sort before every token), then the term postings — the
/// builder's ascending-key contract holds across the seam.
fn write_seg_file(path: &Path, bucket: &kevy_text::cold::FrozenBucket) -> Result<(), String> {
    let mut b = kevy_seg::SegBuilder::create(path).map_err(|e| e.to_string())?;
    for (row_key, payload) in &bucket.fwd {
        let mut k = vec![0u8];
        k.extend_from_slice(row_key);
        b.push(&k, payload).map_err(|e| e.to_string())?;
    }
    for (term, payload) in &bucket.terms {
        b.push(term, payload).map_err(|e| e.to_string())?;
    }
    b.finish().map(|_| ()).map_err(|e| e.to_string())
}

/// Drop a previous run's cold text segments for `index_name` (derived
/// spill: the rebuilt hot index holds everything again).
fn clean_stale(index_name: &[u8], segs_dir: &Path) -> Result<(), String> {
    if !segs_dir.exists() {
        return Ok(());
    }
    let mut m = kevy_seg::Manifest::open(segs_dir).map_err(|e| e.to_string())?;
    let mut tag = TXT_TAG.to_vec();
    tag.extend_from_slice(index_name);
    tag.push(b':');
    let stale: Vec<String> =
        m.live().filter(|e| e.meta.starts_with(&tag)).map(|e| e.file.clone()).collect();
    for f in stale {
        m.drop_seg(&f).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(segs_dir.join(&f));
    }
    Ok(())
}

fn hex_stem(name: &[u8]) -> String {
    name.iter().map(|b| format!("{b:02x}")).collect()
}
