//! Replay-side handling of the `SEGMENTED` stitch frame: re-do the
//! hot-layer eviction the frame records. The rows' write frames
//! precede it in the log — replay brings them into the hot layer, this
//! asks them back out. Idempotent (deleting an absent key is a no-op),
//! and a write frame AFTER it is a revival: hot-first reads shadow the
//! segment copy, so revived rows simply stay.
//!
//! Segment record keys are the hot-layer row keys — the one convention
//! this stitch relies on; a cold form that orders records differently
//! carries the row key where this walk can recover it.

use crate::Store;
use kevy_seg::{Manifest, Seg};
use std::path::Path;

/// Apply one `SEGMENTED <file>` frame against `store`. The manifest in
/// `segs_dir` is the segment set's truth, fsynced before the frame was
/// logged — a frame naming a segment it does not hold means the truth
/// set was damaged afterwards, and the error is a startup refusal (the
/// rows' only durable copy is unreachable; silence would drop them).
///
/// Opens the manifest per frame: frames are per-bucket (one per slide,
/// not per row), so this is a bounded startup cost, not a hot path.
pub fn apply_segmented(store: &mut Store, segs_dir: &Path, file: &[u8]) -> Result<u64, String> {
    let name = str::from_utf8(file).map_err(|_| "SEGMENTED frame names a non-utf8 segment".to_string())?;
    let m = Manifest::open(segs_dir)
        .map_err(|e| format!("segment manifest unreadable at {}: {e}", segs_dir.display()))?;
    if !m.live().any(|e| e.file == name) {
        return Err(format!(
            "AOF says segment '{name}' holds evicted rows, but the manifest at {} does not \
             list it — the segment truth set was damaged after the eviction; restore the \
             segment directory from backup before starting",
            segs_dir.display()
        ));
    }
    let seg = Seg::open(&segs_dir.join(name)).map_err(|e| format!("segment '{name}': {e}"))?;
    let (lo, hi) = (seg.meta().min_key.clone(), seg.meta().max_key.clone());
    let mut evicted = 0u64;
    for r in seg.range(&lo, &hi) {
        let (key, _) = r.map_err(|e| format!("segment '{name}': {e}"))?;
        evicted += store.del(&[key.as_slice()]) as u64;
    }
    Ok(evicted)
}
