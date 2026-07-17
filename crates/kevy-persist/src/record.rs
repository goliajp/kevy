//! The AOF v2 record envelope: `[u32-LE payload_len][u32-LE crc32c][payload]`.
//!
//! One format move buys four things the bare-RESP v1 stream could not give:
//! deterministic resync after a corrupt frame (scan forward, validate
//! len+CRC — false accept ~2⁻³² per candidate), streaming replay (record
//! boundaries without parsing RESP), torn-tail detection without a full
//! parse, and integrity — a flipped payload bit fails the CRC instead of
//! replaying silently (crashgate's payload-flip cell caught v1 doing
//! exactly that). v1 files stay readable forever; a rewrite upgrades them.

use std::io::{self, Write};

use kevy_resp::ArgvView;

use crate::crc32c::crc32c;
use crate::rewrite_fmt::write_multibulk;

/// v2 file magic. Same length as the v1 `KEVYAOF1\n`, so the format sniff
/// reads one fixed-size header.
pub const AOF2_MAGIC: &[u8; 9] = b"KEVYAOF2\n";

/// Which on-disk AOF encoding a file (or an image producer) speaks.
/// v1 = bare RESP stream (3.x and the wasm host-mediated log); v2 = the
/// checksummed record envelope. Readers accept both forever; the engine
/// writes v2 for every new file and upgrades v1 files on rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AofFormat {
    /// Bare RESP command stream (`KEVYAOF1\n` or legacy headerless).
    V1,
    /// Checksummed record envelope (`KEVYAOF2\n`).
    V2,
}

/// Envelope header size: u32 payload length + u32 CRC32C.
pub(crate) const RECORD_HEADER: usize = 8;

/// Sanity cap on a single record's payload (a corrupt length field must not
/// drive a multi-GB allocation): generous for any real command — a 512 MiB
/// value plus framing.
pub(crate) const MAX_RECORD: u32 = 1 << 30;

/// Append one enveloped record wrapping `payload`.
pub(crate) fn write_record<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&crc32c(payload).to_le_bytes())?;
    w.write_all(payload)
}

/// Encode `args` as a RESP multibulk and append it as one enveloped record,
/// reusing `scratch` for the payload bytes (cleared, not shrunk).
pub(crate) fn write_record_multibulk<W: Write, A: ArgvView + ?Sized>(
    w: &mut W,
    args: &A,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    scratch.clear();
    write_multibulk(scratch, args)?;
    write_record(w, scratch)
}

/// One step of a v2 record walk over an in-memory image.
pub(crate) enum RecordStep<'a> {
    /// A complete, checksum-valid record: its payload, and the total bytes
    /// consumed (header + payload).
    Ok { payload: &'a [u8], consumed: usize },
    /// The buffer ends mid-record (torn tail): not an error, the prefix
    /// before this record is the replayable part.
    Truncated,
    /// The record is structurally present but lies: oversized length or a
    /// checksum mismatch. Everything from here on is non-replayable.
    Corrupt(&'static str),
}

/// Inspect the record starting at `buf[pos..]`.
pub(crate) fn next_record(buf: &[u8], pos: usize) -> RecordStep<'_> {
    let rest = &buf[pos..];
    if rest.is_empty() {
        return RecordStep::Truncated; // clean end is the caller's pos == len check
    }
    if rest.len() < RECORD_HEADER {
        return RecordStep::Truncated;
    }
    let len = u32::from_le_bytes(rest[..4].try_into().unwrap());
    if len == 0 || len > MAX_RECORD {
        return RecordStep::Corrupt("record length out of range");
    }
    let crc = u32::from_le_bytes(rest[4..8].try_into().unwrap());
    let Some(payload) = rest.get(RECORD_HEADER..RECORD_HEADER + len as usize) else {
        return RecordStep::Truncated;
    };
    if crc32c(payload) != crc {
        return RecordStep::Corrupt("record checksum mismatch");
    }
    RecordStep::Ok { payload, consumed: RECORD_HEADER + len as usize }
}
