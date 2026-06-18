//! Snapshot ship wire format (T1.22) — split out of [`crate::wire`]
//! to keep that file under the 500-LOC project ceiling.
//!
//! See `docs/snapshot.md` for the full spec. The primary sends:
//!
//!   `+SNAPSHOT\r\n`
//!   `$L1\r\n<L1 bytes>\r\n`  (chunk 1)
//!   `$L2\r\n<L2 bytes>\r\n`  (chunk 2)
//!   ...
//!   `+SNAPSHOT_END <ack_offset>\r\n`
//!
//! Markers are RESP simple strings, chunks are RESP bulk strings —
//! any RESP-aware tool can peek a captured stream.

use crate::wire::{WireError, find_crlf, parse_decimal, push_u64};

/// Per-chunk cap: a chunk's `$L\r\n` length must not exceed this.
/// Replica drops the link if a chunk header reports a larger size.
/// 64 KiB matches a typical TCP segment + keeps the per-chunk
/// allocation modest. The primary may pick any chunk size from
/// `1` up to this.
pub const SNAPSHOT_CHUNK_MAX: usize = 64 * 1024;

/// Maximum length of a snapshot control line (`+SNAPSHOT_END N\r\n`).
/// 256 B is generous — the longest legal line is `+SNAPSHOT_END ` +
/// 20 digits + `\r\n` = 38 B.
pub const SNAPSHOT_LINE_MAX: usize = 256;

/// Decoded snapshot marker, returned by [`decode_snapshot_marker`].
#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotMarker {
    /// `+SNAPSHOT\r\n` — primary is about to stream snapshot chunks.
    Begin,
    /// `+SNAPSHOT_END <ack_offset>\r\n` — end of snapshot; the next
    /// live frame's offset will equal `ack_offset`.
    End(u64),
}

/// Encode the snapshot-begin marker. Allocates the exact 11 bytes.
pub fn encode_snapshot_begin() -> Vec<u8> {
    b"+SNAPSHOT\r\n".to_vec()
}

/// Encode one snapshot chunk as a RESP bulk string. Caller is
/// responsible for chunking — typical strategy is fixed
/// [`SNAPSHOT_CHUNK_MAX`]-sized reads from a snapshot file or
/// in-memory serializer.
///
/// **Debug-asserts** `bytes.len() <= SNAPSHOT_CHUNK_MAX` so an
/// accidental oversize chunk trips during development; release
/// builds emit a frame the peer will reject with [`WireError::BadEnvelope`]
/// (replica's decoder caps incoming chunk lengths).
pub fn encode_snapshot_chunk(bytes: &[u8]) -> Vec<u8> {
    debug_assert!(
        bytes.len() <= SNAPSHOT_CHUNK_MAX,
        "snapshot chunk {} > cap {}",
        bytes.len(),
        SNAPSHOT_CHUNK_MAX,
    );
    let mut out = Vec::with_capacity(16 + bytes.len());
    out.push(b'$');
    push_u64(&mut out, bytes.len() as u64);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
    out
}

/// Encode the snapshot-end marker carrying the ack offset (the
/// next live frame's offset will equal this value).
pub fn encode_snapshot_end(ack_offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(b"+SNAPSHOT_END ");
    push_u64(&mut out, ack_offset);
    out.extend_from_slice(b"\r\n");
    out
}

/// Peek the next line at the front of `buf` to detect a snapshot
/// marker. Returns:
/// - `Ok(Some((marker, used)))` — a full marker line was found;
///   `used` bytes may be dropped.
/// - `Ok(None)` — the buffer doesn't start with a `+` byte; caller
///   should treat the bytes as a regular `*2\r\n` frame and feed
///   the frame decoder instead.
/// - `Err(WireError::Truncated)` — buffer starts with `+` but the
///   `\r\n` terminator is not yet in the buffer.
/// - `Err(WireError::BadEnvelope)` — buffer starts with `+` but the
///   line is neither `+SNAPSHOT` nor `+SNAPSHOT_END <N>`, or the
///   line exceeds [`SNAPSHOT_LINE_MAX`].
pub fn decode_snapshot_marker(buf: &[u8]) -> Result<Option<(SnapshotMarker, usize)>, WireError> {
    if buf.is_empty() {
        return Err(WireError::Truncated);
    }
    if buf[0] != b'+' {
        return Ok(None);
    }
    let Some(eol) = find_crlf(buf, 1) else {
        return if buf.len() > SNAPSHOT_LINE_MAX {
            Err(WireError::BadEnvelope)
        } else {
            Err(WireError::Truncated)
        };
    };
    if eol > SNAPSHOT_LINE_MAX {
        return Err(WireError::BadEnvelope);
    }
    let line = &buf[1..eol];
    if line == b"SNAPSHOT" {
        return Ok(Some((SnapshotMarker::Begin, eol + 2)));
    }
    if let Some(rest) = line.strip_prefix(b"SNAPSHOT_END ") {
        let offset = parse_decimal(rest).ok_or(WireError::BadEnvelope)?;
        return Ok(Some((SnapshotMarker::End(offset), eol + 2)));
    }
    Err(WireError::BadEnvelope)
}

/// Decode the next snapshot chunk (`$L\r\n<L bytes>\r\n`) at the
/// front of `buf`. Returns:
/// - `Ok((chunk_bytes, used))` — `chunk_bytes` borrows from `buf`;
///   `used` bytes were consumed.
/// - `Err(WireError::Truncated)` — not enough bytes for a complete
///   chunk yet.
/// - `Err(WireError::BadEnvelope)` — header wasn't `$L\r\n`, `L`
///   exceeded [`SNAPSHOT_CHUNK_MAX`], `L` parsed as non-numeric, or
///   the trailing CRLF was missing.
pub fn decode_snapshot_chunk(buf: &[u8]) -> Result<(&[u8], usize), WireError> {
    if buf.is_empty() {
        return Err(WireError::Truncated);
    }
    if buf[0] != b'$' {
        return Err(WireError::BadEnvelope);
    }
    let len_eol = find_crlf(buf, 1).ok_or(WireError::Truncated)?;
    let len = parse_decimal(&buf[1..len_eol]).ok_or(WireError::BadEnvelope)?;
    let len = usize::try_from(len).map_err(|_| WireError::BadEnvelope)?;
    if len > SNAPSHOT_CHUNK_MAX {
        return Err(WireError::BadEnvelope);
    }
    let data_start = len_eol + 2;
    let data_end = data_start + len;
    if buf.len() < data_end + 2 {
        return Err(WireError::Truncated);
    }
    if &buf[data_end..data_end + 2] != b"\r\n" {
        return Err(WireError::BadEnvelope);
    }
    Ok((&buf[data_start..data_end], data_end + 2))
}
