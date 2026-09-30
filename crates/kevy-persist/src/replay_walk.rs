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
#[derive(Clone)]
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
    /// Where the open transaction's begin marker starts.
    pub(crate) txn_at: u64,
    /// Zeros from the last record to the end of the file: the unused part
    /// of a mapped log's preallocation, not data.
    pub(crate) zero_tail: u64,
    /// Transactions dropped because the log ended before their commit
    /// marker. Surfaced in the report rather than passed over silently.
    pub(crate) txn_discarded: u64,
}

impl V2Walk {
    /// A walk that has applied nothing yet, positioned at `pos`.
    pub(crate) fn at(pos: u64) -> V2Walk {
        V2Walk {
            txn: None,
            txn_at: 0,
            zero_tail: 0,
            txn_discarded: 0,
            stop: ReplayStop::Clean,
            pos,
            replayed: 0,
            preview: [0u8; 16],
            preview_len: 0,
        }
    }
}

impl V2Walk {
    /// Where the log's settled part ends: before a transaction the log
    /// ended inside of, whose records replay dropped. A log reopened for
    /// appends must continue from here — appended after an open begin
    /// marker, new records would be read as part of that transaction and
    /// dropped with it.
    pub(crate) fn settled_end(&self) -> u64 {
        if self.txn.is_some() { self.txn_at } else { self.pos }
    }
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
    apply: &mut Option<Sink<'_>>,
) -> io::Result<V2Walk> {
    let mut w = V2Walk::at(start_pos);
    let mut payload: Vec<u8> = Vec::new();
    // one argv for the whole walk: its buffers stay warm across frames
    let mut args = Argv::default();
    w.stop = loop {
        let (len, crc) = match next_header(r, &mut w)? {
            Ok(h) => h,
            Err(stop) => break stop,
        };
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
        if !apply_record(&payload, len, &mut args, apply, &mut w) {
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
    args: &mut Argv,
    apply: &mut Option<Sink<'_>>,
    w: &mut V2Walk,
) -> bool {
    match kevy_resp::parse_command_into(payload, args) {
        Ok(Some(used)) if used == payload.len() => {
            let marker = txn_marker(args);
            match marker {
                Some(TxnMarker::Begin) => {
                    // A begin inside a begin cannot happen from this
                    // writer; if a log ever shows one, the outer
                    // transaction was never committed — drop it.
                    if w.txn.take().is_some() {
                        w.txn_discarded += 1;
                    }
                    w.txn = Some(Vec::new());
                    w.txn_at = w.pos;
                }
                Some(TxnMarker::Commit) => {
                    if let Some(buffered) = w.txn.take()
                        && let Some(f) = apply.as_mut()
                    {
                        for mut a in buffered {
                            f.deliver(&mut a);
                        }
                    }
                }
                None => match w.txn.as_mut() {
                    Some(buf) => buf.push(std::mem::take(args)),
                    None => {
                        if let Some(f) = apply.as_mut() {
                            f.deliver(args);
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

/// Where replayed frames go: moved out to an owner, or lent in place, in
/// which case the frame's buffers are reused for the next record.
pub(crate) enum Sink<'a> {
    Owned(&'a mut dyn FnMut(Argv)),
    InPlace(&'a mut dyn FnMut(&mut Argv)),
}

impl Sink<'_> {
    pub(crate) fn deliver(&mut self, frame: &mut Argv) {
        match self {
            Sink::Owned(f) => f(std::mem::take(frame)),
            Sink::InPlace(f) => f(frame),
        }
    }
}

/// Read the next record header: its `(len, crc)`, or why the walk stops
/// here — a clean end, a zero tail, a torn or impossible header.
fn next_header(r: &mut impl Read, w: &mut V2Walk) -> io::Result<Result<(u32, u32), ReplayStop>> {
    let mut header = [0u8; 8];
    match read_fully(r, &mut header)? {
        0 => return Ok(Err(ReplayStop::Clean)),
        n if n < header.len() && header[..n].iter().all(|&b| b == 0) => {
            w.zero_tail = n as u64;
            return Ok(Err(ReplayStop::Clean));
        }
        n if n < header.len() => return Ok(Err(ReplayStop::TruncatedTail)),
        _ => {}
    }
    let len = u32::from_le_bytes(header[..4].try_into().expect("header is a fixed-size array"));
    let crc = u32::from_le_bytes(header[4..].try_into().expect("header is a fixed-size array"));
    // no record has length 0, so zeros from here to the end of the file
    // are the unused preallocation of a mapped log; zeros followed by
    // anything else are damage
    if len == 0
        && crc == 0
        && let Some(rest) = zeros_to_end(r)?
    {
        w.zero_tail = header.len() as u64 + rest;
        return Ok(Err(ReplayStop::Clean));
    }
    if len == 0 || len > crate::record::MAX_RECORD {
        w.preview_len = preview_of(&header, &mut w.preview);
        return Ok(Err(ReplayStop::CorruptFrame(String::from("record length out of range"))));
    }
    Ok(Ok((len, crc)))
}

/// Read `r` to its end; `Some(bytes read)` when every one was zero.
fn zeros_to_end(r: &mut impl Read) -> io::Result<Option<u64>> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let got = read_fully(r, &mut buf)?;
        if buf[..got].iter().any(|&b| b != 0) {
            return Ok(None);
        }
        n += got as u64;
        if got < buf.len() {
            return Ok(Some(n));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A read that fails for a reason other than the end fails the walk:
    /// on the header itself, or while checking that zeros run to the end.
    #[test]
    fn a_failing_read_is_an_error_not_an_end() {
        let dir = kevy_tmpdir::TmpDir::new("walk-read-dir");
        let not_a_file = || std::fs::File::open(dir.path()).unwrap();
        let mut w = V2Walk::at(0);
        assert!(next_header(&mut not_a_file(), &mut w).is_err());
        let mut zeros_then_fail = (&[0u8; 8][..]).chain(not_a_file());
        assert!(next_header(&mut zeros_then_fail, &mut w).is_err());
        assert_eq!(w.zero_tail, 0, "an unread tail is not counted as zeros");
    }
}
