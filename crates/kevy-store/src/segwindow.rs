//! Replay-side handling of the `SEGMENTED` stitch frame: re-establish
//! every row the named segment holds as a seg-backed stub. The rows'
//! write frames precede the stitch in the log — replay brings them in
//! hot and the stitch phase-changes them; a row the log never rebuilt
//! (a rewritten log carries no cold-row commands) is inserted as a
//! stub directly from the segment. A write frame AFTER the stitch is
//! a revival: promote-then-write replaces the stub and the segment
//! record strands. Idempotent — an already-stubbed row is left alone.

use crate::Store;
use crate::value::Value;

/// Apply one `SEGMENTED <file>` frame against `store`. The manifest in
/// `segs_dir` is the segment set's truth, fsynced before the frame was
/// logged — a frame naming a segment it does not hold means the truth
/// set was damaged afterwards, and the error is a startup refusal (the
/// rows' only durable copy is unreachable; silence would drop them).
pub fn apply_segmented(
    store: &mut Store,
    segs_dir: &std::path::Path,
    file: &[u8],
) -> Result<u64, String> {
    let name =
        str::from_utf8(file).map_err(|_| "SEGMENTED frame names a non-utf8 segment".to_string())?;
    store.enable_seg_rows(segs_dir)?;
    let Some(seq) = store.row_seg_seq(name) else {
        return Err(format!(
            "AOF says segment '{name}' holds evicted rows, but the manifest at {} does not \
             list it — the segment truth set was damaged after the eviction; restore the \
             segment directory from backup before starting",
            segs_dir.display()
        ));
    };
    let mut stitched = 0u64;
    let records = store.row_seg_records(seq);
    for (key, payload) in records {
        match store.peek_value_kind(&key) {
            RowState::Hot => {
                if store.demote_row_to_seg(&key, seq) {
                    stitched += 1;
                }
            }
            RowState::Absent => {
                let weight = crate::tier_codec::decode(crate::value::COLD_TAG_HASH, payload)
                    .map_err(|e| format!("segment '{name}': {e}"))?
                    .weight();
                store.insert_row_stub(&key, seq, weight);
                stitched += 1;
            }
            RowState::AlreadyCold | RowState::OtherType => {}
        }
    }
    store.note_stitched(seq, stitched);
    Ok(stitched)
}

/// What replay found under a stitched key.
pub(crate) enum RowState {
    Hot,
    Absent,
    AlreadyCold,
    OtherType,
}

impl Store {
    pub(crate) fn peek_value_kind(&mut self, key: &[u8]) -> RowState {
        match self.live_entry(key).map(|e| &e.value) {
            None => RowState::Absent,
            Some(Value::Hash(_) | Value::SmallHashInline(_)) => RowState::Hot,
            Some(Value::Cold(_)) => RowState::AlreadyCold,
            Some(_) => RowState::OtherType,
        }
    }
}
