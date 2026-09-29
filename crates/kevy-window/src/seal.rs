//! Sealing a slide's batch into a manifest-registered segment, and
//! dropping a previous run's spill for the same index.

use std::path::Path;

use kevy_index::{IndexValue, encode_seg_values, seg_key};

use crate::{ColdError, WindowRt};

impl WindowRt {
    /// `KEVY_PROBE_SLIDE=1`: one line per slide with what was sealed,
    /// what left the tree, and how many shadows are outstanding.
    ///
    /// This is the instrument that found the stale-tombstone loss. The
    /// first three numbers refute the obvious theory (the seal drops
    /// what arrives mid-build — it does not; sealed always equals
    /// split_off), which is what left the tombstone count as the only
    /// remaining place the missing rows could be.
    pub(super) fn probe(&self, index_name: &[u8], split_off: usize) {
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
    pub(super) fn build_segment(
        &mut self,
        index_name: &[u8],
        seg: &kevy_index::Segment,
        bound: &IndexValue,
        segs_dir: &Path,
    ) -> Result<String, ColdError> {
        std::fs::create_dir_all(segs_dir).map_err(ColdError::Io)?;
        let file = format!("idx-{}-{}.seg", hex_stem(index_name), self.seq);
        self.seq += 1;
        let path = segs_dir.join(&file);
        let build = || -> Result<kevy_seg::SegMeta, ColdError> {
            let mut b = kevy_seg::SegBuilder::create(&path)?;
            let mut below = seg.scan_below(bound);
            while let Some((v, k)) = below.next_entry() {
                let key = seg_key(v, k);
                // The payload carries the row's stored VALUES so the
                // clause-carrying cold path never re-reads the row
                // (which may itself have gone cold). No declared
                // values = the empty payload, the a-train shape.
                let vals = below.stored_row();
                let refs: Vec<Option<&[u8]>> = vals.iter().map(|v| v.as_deref()).collect();
                b.push(&key, &encode_seg_values(&refs))?;
            }
            Ok(b.finish()?)
        };
        let meta = build().inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;
        let mut m = kevy_seg::Manifest::open(segs_dir)?;
        m.add(
            kevy_seg::ManifestEntry::new(file.clone(), meta)
                .with_meta([b"idxcold:", index_name].concat()),
        )?;
        Ok(file)
    }
}

/// Drop a previous run's derived segments for `index_name`: their
/// manifest entries unregister first, then the files unlink (the
/// ledger never points at nothing).
pub(super) fn clean_stale_derived(index_name: &[u8], segs_dir: &Path) -> Result<(), ColdError> {
    if !segs_dir.exists() {
        return Ok(());
    }
    let mut m = kevy_seg::Manifest::open(segs_dir)?;
    let tag = [b"idxcold:", index_name].concat();
    let stale: Vec<String> = m.live().filter(|e| e.meta == tag).map(|e| e.file.clone()).collect();
    for f in stale {
        m.drop_seg(&f)?;
        let _ = std::fs::remove_file(segs_dir.join(&f));
    }
    Ok(())
}

/// Index names are free bytes; the segment file name needs a safe
/// stem. Hex is unambiguous and the manifest carries the real name.
fn hex_stem(name: &[u8]) -> String {
    name.iter().map(|b| format!("{b:02x}")).collect()
}
