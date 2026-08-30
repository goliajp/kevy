//! The byte-level walk over v2 frames.
//!
//! Split out of `replay.rs` when that file hit the workspace's 500-line
//! ceiling for the third time in a day — the third trip is the signal to
//! cut, not to shave another comment. The boundary is the one that was
//! already there: this file decides what a stretch of bytes IS, and
//! `replay.rs` decides what to do about it and what to report.

use std::io::{self, Read};

use kevy_resp::Argv;

use crate::replay_txn::{TxnMarker, txn_marker};

/// Outcome of an AOF replay run — drives the summary log shape (rendered
/// in `replay_log.rs`).
pub(crate) enum ReplayStop {
    Clean,
    TruncatedTail,
    /// A record's length field claimed `claimed` bytes with only `available`
    /// left in the file. EOF really was hit, so this is a short read — but a
    /// partial frame from a crash mid-append is at most one record, and a
    /// claim of megabytes with megabytes behind it is corruption wearing
    /// truncation's clothes. Separated from `TruncatedTail` so the operator
    /// message stops calling it "a partial frame (crash mid-append,
    /// recoverable)": CI printed exactly that for 27,485,178 bytes.
    LengthOutranFile {
        claimed: u64,
        available: u64,
    },
    CorruptFrame(String),
}

// The summary line lives in `replay_log.rs` (500-LOC split); the corrupt
// WARN branch is unconditional there, the informational branches honor
// `quiet_info`.

/// Outcome of the streaming v2 record walk.
pub(crate) struct V2Walk {
    pub(crate) stop: ReplayStop,
    /// Absolute offset after the last applied record.
    pub(crate) pos: u64,
    pub(crate) replayed: u64,
    pub(crate) preview: [u8; 16],
    pub(crate) preview_len: usize,
    /// Frames seen since a transaction's begin marker, held back until
    /// its commit marker arrives. `None` = not inside a transaction.
    ///
    /// This is where atomicity actually happens. Group commit only
    /// defers the fsync; frames still reach the kernel when the write
    /// buffer fills, so a crash inside a transaction bigger than that
    /// buffer leaves whole, valid, individually-replayable frames on
    /// disk — measured at 6393/20000. Holding them until the commit
    /// marker makes "was this transaction finished" a property of the
    /// log rather than of how much of it happened to be flushed.
    pub(crate) txn: Option<Vec<Argv>>,
    /// Transactions dropped because the log ended before their commit
    /// marker. Surfaced in the report rather than passed over silently.
    pub(crate) txn_discarded: u64,
}

/// Capture up to 16 bytes of the offending bytes for the WARN preview.
pub(crate) fn preview_of(bytes: &[u8], out: &mut [u8; 16]) -> usize {
    let n = bytes.len().min(out.len());
    out[..n].copy_from_slice(&bytes[..n]);
    n
}

/// A length field the file could not honour.
fn outran(claimed: u32, available: usize) -> ReplayStop {
    ReplayStop::LengthOutranFile { claimed: u64::from(claimed), available: available as u64 }
}

/// The sequential record walk: read `[len][crc][payload]` envelopes from
/// `r`, verify, apply. Peak memory is O(largest record) — the streaming
/// property the whole v2 replay exists for.
pub(crate) fn walk_v2(
    r: &mut impl Read,
    start_pos: u64,
    apply: &mut Option<&mut dyn FnMut(Argv)>,
) -> io::Result<V2Walk> {
    let mut w = V2Walk {
        txn: None,
        txn_discarded: 0,
        stop: ReplayStop::Clean,
        pos: start_pos,
        replayed: 0,
        preview: [0u8; 16],
        preview_len: 0,
    };
    let mut payload: Vec<u8> = Vec::new();
    w.stop = loop {
        let mut header = [0u8; 8];
        match read_fully(r, &mut header) {
            Ok(0) => break ReplayStop::Clean,
            Ok(n) if n < header.len() => break ReplayStop::TruncatedTail,
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(header[..4].try_into().unwrap());
        let crc = u32::from_le_bytes(header[4..].try_into().unwrap());
        if len == 0 || len > crate::record::MAX_RECORD {
            w.preview_len = preview_of(&header, &mut w.preview);
            break ReplayStop::CorruptFrame(String::from("record length out of range"));
        }
        payload.clear();
        payload.resize(len as usize, 0);
        match read_fully(r, &mut payload) {
            Ok(n) if n < payload.len() => break outran(len, n),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        if crate::crc32c::crc32c(&payload) != crc {
            w.preview_len = preview_of(&payload, &mut w.preview);
            break ReplayStop::CorruptFrame(String::from("record checksum mismatch"));
        }
        if !apply_record(&payload, len, apply, &mut w) {
            break ReplayStop::CorruptFrame(String::from(
                "checksummed record does not hold exactly one command",
            ));
        }
    };
    Ok(w)
}

/// Parse-and-apply one checksum-valid record payload; `false` = the
/// payload is not exactly one command (a lying record).
pub(crate) fn apply_record(
    payload: &[u8],
    len: u32,
    apply: &mut Option<&mut dyn FnMut(Argv)>,
    w: &mut V2Walk,
) -> bool {
    match kevy_resp::parse_command(payload) {
        Ok(Some((args, used))) if used == payload.len() => {
            let marker = txn_marker(&args);
            match marker {
                Some(TxnMarker::Begin) => {
                    // A begin inside a begin cannot happen from this
                    // writer; if a log ever shows one, the outer
                    // transaction was never committed — drop it.
                    if w.txn.take().is_some() {
                        w.txn_discarded += 1;
                    }
                    w.txn = Some(Vec::new());
                }
                Some(TxnMarker::Commit) => {
                    if let Some(buffered) = w.txn.take()
                        && let Some(f) = apply.as_deref_mut()
                    {
                        for a in buffered {
                            f(a);
                        }
                    }
                }
                None => match w.txn.as_mut() {
                    Some(buf) => buf.push(args),
                    None => {
                        if let Some(f) = apply.as_deref_mut() {
                            f(args);
                        }
                    }
                },
            }
            w.pos += 8 + u64::from(len);
            w.replayed += 1;
            true
        }
        _ => {
            w.preview_len = preview_of(payload, &mut w.preview);
            false
        }
    }
}

/// `read_exact` that reports a clean-vs-partial short read instead of
/// erroring: returns the bytes actually read (0 = clean EOF at a record
/// boundary, partial = torn tail).
pub(crate) fn read_fully<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}
