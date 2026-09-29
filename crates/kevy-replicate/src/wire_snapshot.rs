//! Snapshot ship wire format — split out of [`crate::wire`]
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

use crate::feed::FeedPosition;
use crate::wire::{WireError, find_crlf, parse_decimal, push_u64};

/// Per-chunk cap: a chunk's `$L\r\n` length must not exceed this.
/// Replica drops the link if a chunk header reports a larger size.
/// 64 KiB matches a typical TCP segment + keeps the per-chunk
/// allocation modest. The primary may pick any chunk size from
/// `1` up to this.
///
/// ```
/// use kevy_replicate::wire::{SNAPSHOT_CHUNK_MAX, WireError, decode_snapshot_chunk};
///
/// let dump = vec![0u8; 3 * SNAPSHOT_CHUNK_MAX / 2];
/// assert_eq!(dump.chunks(SNAPSHOT_CHUNK_MAX).count(), 2); // ship it in two chunks
/// // a header announcing a larger chunk is refused before any allocation
/// let oversize = format!("${}\r\n", SNAPSHOT_CHUNK_MAX + 1);
/// assert_eq!(decode_snapshot_chunk(oversize.as_bytes()), Err(WireError::BadEnvelope));
/// ```
pub const SNAPSHOT_CHUNK_MAX: usize = 64 * 1024;

/// Maximum length of a snapshot control line (`+SNAPSHOT_END N\r\n`).
/// 256 B is generous — the longest legal line is `+SNAPSHOT_END ` +
/// 20 digits + `\r\n` = 38 B.
///
/// ```
/// use kevy_replicate::wire::{SNAPSHOT_LINE_MAX, WireError, decode_snapshot_marker};
///
/// assert!(kevy_replicate::wire::encode_snapshot_end(u64::MAX).len() <= SNAPSHOT_LINE_MAX);
/// // a `+` line that runs on without a terminator is refused, not buffered forever
/// let runaway = [b"+".as_slice(), &[b'x'; SNAPSHOT_LINE_MAX + 1]].concat();
/// assert_eq!(decode_snapshot_marker(&runaway), Err(WireError::BadEnvelope));
/// ```
pub const SNAPSHOT_LINE_MAX: usize = 256;

/// Decoded snapshot marker, returned by [`decode_snapshot_marker`].
///
/// ```
/// use kevy_replicate::wire::{SnapshotMarker, decode_snapshot_marker, encode_snapshot_end};
///
/// let (marker, _) = decode_snapshot_marker(&encode_snapshot_end(12))?.expect("a marker line");
/// let next_live_offset = match marker {
///     SnapshotMarker::End(ack) => ack,
///     other => panic!("unexpected {other:?}"),
/// };
/// assert_eq!(next_live_offset, 12);
/// # Ok::<(), kevy_replicate::wire::WireError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SnapshotMarker {
    /// `+SNAPSHOT\r\n` — primary is about to stream snapshot chunks.
    ///
    /// ```
    /// use kevy_replicate::wire::{SnapshotMarker, decode_snapshot_marker};
    ///
    /// assert_eq!(decode_snapshot_marker(b"+SNAPSHOT\r\n")?, Some((SnapshotMarker::Begin, 11)));
    /// # Ok::<(), kevy_replicate::wire::WireError>(())
    /// ```
    Begin,
    /// `+SNAPSHOT_END <ack_offset>\r\n` — end of snapshot; the next
    /// live frame's offset will equal `ack_offset`.
    ///
    /// ```
    /// use kevy_replicate::wire::{SnapshotMarker, decode_snapshot_marker};
    ///
    /// let line = b"+SNAPSHOT_END 42\r\n";
    /// assert_eq!(decode_snapshot_marker(line)?, Some((SnapshotMarker::End(42), line.len())));
    /// # Ok::<(), kevy_replicate::wire::WireError>(())
    /// ```
    End(u64),
    /// `+PING <generation> <next_offset>\r\n` — in-stream
    /// heartbeat: the primary's current feed generation + `next_offset`,
    /// sent every ~1s so a replica can compute its own lag (applied vs
    /// primary) and judge link liveness without a request/response
    /// round trip. Out-of-band: occupies no offset space.
    ///
    /// The generation lets REPL.TOKEN / REPL.WAIT compare
    /// token generations against the replica's last-seen upstream
    /// generation. A legacy one-number `+PING <next_offset>\r\n` line
    /// still decodes — `generation` reads as `0`, the "unknown" value
    /// no real feed ever serves (feed generations start at 1).
    ///
    /// ```
    /// use kevy_replicate::feed::FeedPosition;
    /// use kevy_replicate::wire::{SnapshotMarker, decode_snapshot_marker};
    ///
    /// let (ping, _) = decode_snapshot_marker(b"+PING 3 120\r\n")?.expect("a marker line");
    /// assert_eq!(ping, SnapshotMarker::Ping(FeedPosition::new(3, 120)));
    /// let (legacy, _) = decode_snapshot_marker(b"+PING 120\r\n")?.expect("a marker line");
    /// assert_eq!(legacy, SnapshotMarker::Ping(FeedPosition::new(0, 120))); // generation unknown
    /// # Ok::<(), kevy_replicate::wire::WireError>(())
    /// ```
    Ping(FeedPosition),
}

/// Encode the snapshot-begin marker. Allocates the exact 11 bytes.
///
/// ```
/// assert_eq!(kevy_replicate::wire::encode_snapshot_begin(), b"+SNAPSHOT\r\n");
/// ```
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
///
/// ```
/// use kevy_replicate::wire::{SNAPSHOT_CHUNK_MAX, decode_snapshot_chunk, encode_snapshot_chunk};
///
/// let dump = b"serialized keyspace";
/// let mut stream = Vec::new();
/// for part in dump.chunks(SNAPSHOT_CHUNK_MAX) {
///     stream.extend(encode_snapshot_chunk(part));
/// }
/// assert_eq!(stream, b"$19\r\nserialized keyspace\r\n");
/// assert_eq!(decode_snapshot_chunk(&stream)?.0, dump);
/// # Ok::<(), kevy_replicate::wire::WireError>(())
/// ```
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

/// Encode the in-stream heartbeat carrying the primary's tail:
/// `+PING <generation> <next_offset>\r\n`.
///
/// ```
/// use kevy_replicate::feed::FeedPosition;
///
/// assert_eq!(kevy_replicate::wire::encode_ping(FeedPosition::new(3, 9)), b"+PING 3 9\r\n");
/// ```
pub fn encode_ping(tail: FeedPosition) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    out.extend_from_slice(b"+PING ");
    push_u64(&mut out, tail.generation);
    out.push(b' ');
    push_u64(&mut out, tail.offset);
    out.extend_from_slice(b"\r\n");
    out
}

/// Encode the replica→primary acknowledgment line:
/// `REPLCONF ACK <offset>\r\n` — inline RESP, written back on the
/// SAME replication connection (the pump drains it non-blocking).
///
/// ```
/// use kevy_replicate::wire::{decode_replconf_ack, encode_replconf_ack};
///
/// let line = encode_replconf_ack(128);
/// assert_eq!(line, b"REPLCONF ACK 128\r\n");
/// assert_eq!(decode_replconf_ack(&line)?, Some((128, line.len())));
/// # Ok::<(), kevy_replicate::wire::WireError>(())
/// ```
pub fn encode_replconf_ack(offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(b"REPLCONF ACK ");
    push_u64(&mut out, offset);
    out.extend_from_slice(b"\r\n");
    out
}

/// Parse one `REPLCONF ACK <offset>\r\n` line at the front of `buf`.
/// `Ok(Some((offset, used)))` on a full line; `Ok(None)` if the buffer
/// doesn't start with `R` (not an ACK); `Err(Truncated)` if incomplete.
///
/// ```
/// use kevy_replicate::wire::{WireError, decode_replconf_ack};
///
/// assert_eq!(decode_replconf_ack(b"REPLCONF ACK 9\r\n")?, Some((9, 16)));
/// assert_eq!(decode_replconf_ack(b"PING\r\n")?, None); // not an ack
/// assert_eq!(decode_replconf_ack(b"REPLCONF AC"), Err(WireError::Truncated));
/// # Ok::<(), WireError>(())
/// ```
pub fn decode_replconf_ack(buf: &[u8]) -> Result<Option<(u64, usize)>, WireError> {
    if buf.is_empty() {
        return Err(WireError::Truncated);
    }
    if buf[0] != b'R' {
        return Ok(None);
    }
    let Some(eol) = find_crlf(buf, 0) else {
        return if buf.len() > SNAPSHOT_LINE_MAX {
            Err(WireError::BadEnvelope)
        } else {
            Err(WireError::Truncated)
        };
    };
    let line = &buf[..eol];
    let Some(rest) = line.strip_prefix(b"REPLCONF ACK ") else {
        return Err(WireError::BadEnvelope);
    };
    let offset = parse_decimal(rest).ok_or(WireError::BadEnvelope)?;
    Ok(Some((offset, eol + 2)))
}

/// Encode the snapshot-end marker carrying the ack offset (the
/// next live frame's offset will equal this value).
///
/// ```
/// assert_eq!(kevy_replicate::wire::encode_snapshot_end(57), b"+SNAPSHOT_END 57\r\n");
/// ```
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
///
/// ```
/// use kevy_replicate::wire::{SnapshotMarker, WireError, decode_snapshot_marker};
///
/// assert_eq!(decode_snapshot_marker(b"+SNAPSHOT\r\n$3")?, Some((SnapshotMarker::Begin, 11)));
/// assert_eq!(decode_snapshot_marker(b"*2\r\n")?, None); // a live frame: use decode_frame
/// assert_eq!(decode_snapshot_marker(b"+SNAPSH"), Err(WireError::Truncated));
/// assert_eq!(decode_snapshot_marker(b"+HELLO\r\n"), Err(WireError::BadEnvelope));
/// # Ok::<(), WireError>(())
/// ```
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
    if let Some(rest) = line.strip_prefix(b"PING ") {
        // Two-number form: `<generation> <next_offset>`.
        // One-number legacy form: `<next_offset>` — decodes
        // with generation 0 ("unknown"; real generations start at 1).
        let marker = match rest.iter().position(|&b| b == b' ') {
            Some(sp) => {
                let generation = parse_decimal(&rest[..sp]).ok_or(WireError::BadEnvelope)?;
                let next_offset = parse_decimal(&rest[sp + 1..]).ok_or(WireError::BadEnvelope)?;
                SnapshotMarker::Ping(FeedPosition::new(generation, next_offset))
            }
            None => SnapshotMarker::Ping(FeedPosition::new(
                0,
                parse_decimal(rest).ok_or(WireError::BadEnvelope)?,
            )),
        };
        return Ok(Some((marker, eol + 2)));
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
///
/// ```
/// use kevy_replicate::wire::{WireError, decode_snapshot_chunk};
///
/// let (chunk, used) = decode_snapshot_chunk(b"$4\r\nabcd\r\n+SNAPSHOT_END 9\r\n")?;
/// assert_eq!((chunk, used), (&b"abcd"[..], 10)); // the end marker follows
/// assert_eq!(decode_snapshot_chunk(b"$4\r\nab"), Err(WireError::Truncated));
/// # Ok::<(), WireError>(())
/// ```
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
