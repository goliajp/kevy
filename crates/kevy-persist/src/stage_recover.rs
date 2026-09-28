//! What a staging ring left behind is worth, decided at open once the AOF
//! has been replayed and its torn tail repaired.
//!
//! A drain writes the records to the AOF first and moves the ring's header
//! second, so a process killed between the two leaves the AOF holding
//! records the header still calls undrained. Those bytes are recognised by
//! comparing them, not assumed: the ring's first undrained records must be
//! byte-for-byte the AOF's tail past the length the header recorded.

use crate::stage_ring::{StageHead, StageRing};

/// The verdict on a ring found at open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// Nothing in the ring is owed to the store, for the reason given.
    Discard(&'static str),
    /// These whole AOF records were committed and never reached the AOF:
    /// replay them, append them to the log, then reset the ring. `torn` is
    /// true when the walk stopped at a record that failed its checksum
    /// before reaching the commit offset — only a power loss writes pages
    /// out of order like that.
    Replay {
        /// The records, in order, each a whole v2 record.
        records: Vec<Vec<u8>>,
        /// The walk stopped short of the commit offset.
        torn: bool,
    },
}

/// Decide what the ring owes a log whose id, taken over the span the
/// ring's id names, is `log_id`, with `aof_len` valid bytes, of which
/// `aof_tail` are the ones past `head.aof_len`.
pub(crate) fn recover(
    ring: &StageRing,
    head: StageHead,
    log_id: u64,
    aof_len: u64,
    aof_tail: &[u8],
) -> Recovery {
    if head.log_id != log_id {
        // a rewrite or reset replaced the log after the ring's last rebase;
        // the new file already reflects every record the ring held
        return Recovery::Discard("the ring continues another log");
    }
    if aof_len < head.aof_len {
        return Recovery::Discard("the log is shorter than the ring last saw it");
    }
    if aof_len - head.aof_len != aof_tail.len() as u64 {
        return Recovery::Discard("the log's tail was not handed over whole");
    }
    let mut pos = head.drained;
    let mut matched = 0usize;
    while matched < aof_tail.len() {
        let Some((start, len)) = ring.record_at(pos, head.commit) else {
            return Recovery::Discard("the log holds bytes the ring never had");
        };
        let end = matched + len as usize;
        if end > aof_tail.len() || ring.logical(start, len) != &aof_tail[matched..end] {
            return Recovery::Discard("the log's tail is not the ring's first records");
        }
        matched = end;
        pos = start + len;
    }
    let mut records = Vec::new();
    while let Some((start, len)) = ring.record_at(pos, head.commit) {
        if !ring.record_intact(start, len) {
            break;
        }
        records.push(ring.logical(start, len).to_vec());
        pos = start + len;
    }
    // a wrap marker is committed together with the record after it, so a
    // walk that stops short of the commit offset stopped at a bad record
    Recovery::Replay { records, torn: pos != head.commit }
}
