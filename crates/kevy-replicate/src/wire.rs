//! Wire format for replicated mutations — see `docs/wire.md` for the
//! full spec.
//!
//! Each frame is `*2\r\n:<offset>\r\n<RESP2 multi-bulk argv>`. The
//! envelope is itself a valid RESP2 array of 2 elements, so any
//! RESP-aware debug tool can peek a captured stream. The inner argv
//! payload is byte-identical to what a client would have sent when
//! issuing the same command, so feeding it through the existing
//! [`parse_command_into`] reconstructs the same [`Argv`] the primary
//! applied.
//!
//! ```
//! use kevy_replicate::wire::{decode_frame, encode_frame};
//!
//! let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
//! let bytes = encode_frame(7, &set);
//! assert!(bytes.starts_with(b"*2\r\n:7\r\n")); // envelope, then the offset
//! // the payload is the request a client would have sent
//! let mut argv = kevy_resp::Argv::default();
//! kevy_resp::parse_command_into(&bytes[8..], &mut argv)?;
//! assert_eq!(argv, set);
//! assert_eq!(decode_frame(&bytes)?.0.argv, set);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use crate::replica::DecodedFrame;
use kevy_resp::{Argv, ArgvView, ProtocolError, parse_command_into};

// Snapshot ship helpers live in [`crate::wire_snapshot`] (split out
// to keep this file under the 500-LOC project ceiling); re-export
// here so the canonical import path stays `kevy_replicate::wire::*`.
pub use crate::wire_snapshot::{
    SNAPSHOT_CHUNK_MAX, SNAPSHOT_LINE_MAX, SnapshotMarker, decode_replconf_ack,
    decode_snapshot_chunk, decode_snapshot_marker, encode_ping, encode_replconf_ack,
    encode_snapshot_begin, encode_snapshot_chunk, encode_snapshot_end,
};

/// Wire-layer error. Only [`WireError::Truncated`] is recoverable by
/// the caller (read more bytes and retry); the other variants signal
/// a corrupt or protocol-violating peer and call for dropping the
/// connection.
///
/// ```
/// use kevy_replicate::wire::{WireError, decode_frame, encode_frame};
///
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// let bytes = encode_frame(0, &set);
/// // a partial read: keep the bytes and read more
/// assert_eq!(decode_frame(&bytes[..10]).unwrap_err(), WireError::Truncated);
/// // garbage: drop the peer
/// let err = decode_frame(b"+OK\r\n").unwrap_err();
/// assert_eq!(err, WireError::BadEnvelope);
/// assert_eq!(err.to_string(), "wire envelope not *2");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WireError {
    /// Buffer ended before a complete frame; accumulate more bytes
    /// and call [`decode_frame`] again.
    ///
    /// ```
    /// use kevy_replicate::wire::{WireError, decode_frame, encode_frame};
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let bytes = encode_frame(3, &set);
    /// let (mut buf, mut decoded) = (Vec::new(), None);
    /// for chunk in bytes.chunks(8) {
    ///     buf.extend_from_slice(chunk); // bytes arrive a few at a time
    ///     match decode_frame(&buf) {
    ///         Err(WireError::Truncated) => continue,
    ///         Ok((frame, used)) => decoded = Some((frame.offset, used)),
    ///         Err(e) => panic!("{e}"),
    ///     }
    /// }
    /// assert_eq!(decoded, Some((3, bytes.len())));
    /// ```
    Truncated,
    /// Outer envelope did not start with `*2\r\n` (the only legal
    /// envelope length).
    ///
    /// ```
    /// use kevy_replicate::wire::{WireError, decode_frame};
    ///
    /// // a three-element envelope is not a frame
    /// let err = decode_frame(b"*3\r\n:1\r\n*0\r\n:0\r\n").unwrap_err();
    /// assert_eq!(err, WireError::BadEnvelope);
    /// ```
    BadEnvelope,
    /// Offset element did not parse as a RESP integer (`:N\r\n`).
    ///
    /// ```
    /// use kevy_replicate::wire::{WireError, decode_frame};
    ///
    /// // the offset arrives as a bulk string instead of an integer
    /// let err = decode_frame(b"*2\r\n$2\r\n42\r\n*1\r\n$4\r\nPING\r\n").unwrap_err();
    /// assert_eq!(err, WireError::BadOffset);
    /// ```
    BadOffset,
    /// RESP integer parsed but is negative. Offsets are `u64`.
    ///
    /// ```
    /// use kevy_replicate::wire::{WireError, decode_frame};
    ///
    /// let err = decode_frame(b"*2\r\n:-7\r\n*1\r\n$4\r\nPING\r\n").unwrap_err();
    /// assert_eq!(err, WireError::NegativeOffset(-7));
    /// ```
    NegativeOffset(i64),
    /// Inner multi-bulk argv was malformed at the RESP layer.
    ///
    /// ```
    /// use std::error::Error;
    /// use kevy_replicate::wire::{WireError, decode_frame};
    ///
    /// let err = decode_frame(b"*2\r\n:1\r\n*1\r\n!nope\r\n").unwrap_err();
    /// assert!(matches!(err, WireError::BadPayload(_)));
    /// assert!(err.source().is_some()); // the RESP parser's own error
    /// ```
    BadPayload(ProtocolError),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "wire frame truncated"),
            Self::BadEnvelope => write!(f, "wire envelope not *2"),
            Self::BadOffset => write!(f, "wire offset element not RESP integer"),
            Self::NegativeOffset(n) => write!(f, "wire offset is negative: {n}"),
            Self::BadPayload(e) => write!(f, "wire inner payload malformed: {e}"),
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BadPayload(e) => Some(e),
            _ => None,
        }
    }
}

/// Encode one replication frame: outer `*2`, offset integer, then the
/// argv as a RESP2 multi-bulk request. Allocates a fresh `Vec<u8>`.
/// Generic over [`ArgvView`] so the hot path can pass a borrowed argv
/// straight from the dispatcher (no Argv materialisation per write).
///
/// See `docs/wire.md` for the byte layout.
///
/// `offset` must fit in [`i64::MAX`] — the wire envelope uses a RESP
/// integer for the offset, which is signed by spec. `i64::MAX` is 9.2
/// exabytes of frames; at 10M writes/s that is ~30,000 years, so no
/// real deployment is at risk. In debug builds we assert; release
/// builds emit a frame the peer will reject with `BadOffset`.
///
/// ```
/// use kevy_replicate::wire::encode_frame;
///
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// assert_eq!(encode_frame(99, &set), b"*2\r\n:99\r\n*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
/// ```
pub fn encode_frame<A: ArgvView + ?Sized>(offset: u64, argv: &A) -> Vec<u8> {
    debug_assert!(
        i64::try_from(offset).is_ok(),
        "replication offset {offset} exceeds i64::MAX — wire envelope cannot encode",
    );
    // Pre-size: outer header (~5) + offset line (≤22) + inner array
    // header (~6) + argv buf bytes + per-arg `$N\r\n` headers (~8 each)
    // + trailing CRLF per arg (2 each). Slight overshoot is fine.
    let est = 32 + argv_byte_estimate_view(argv);
    let mut out = Vec::with_capacity(est);
    // Envelope: 2 elements.
    out.extend_from_slice(b"*2\r\n");
    // Element 1: offset as RESP integer.
    out.push(b':');
    push_u64(&mut out, offset);
    out.extend_from_slice(b"\r\n");
    // Element 2: RESP2 multi-bulk argv (byte-identical to a client
    // request, so the receiver feeds it through parse_command_into).
    let n = argv.len();
    out.push(b'*');
    push_u64(&mut out, n as u64);
    out.extend_from_slice(b"\r\n");
    for i in 0..n {
        let arg = &argv[i];
        out.push(b'$');
        push_u64(&mut out, arg.len() as u64);
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Decode the first complete frame at the front of `buf`.
///
/// Returns the frame and the number of bytes it consumed (advance the
/// caller's read cursor by that much). On [`WireError::Truncated`], the caller should read more
/// bytes and retry; any other error signals an unrecoverable peer
/// violation.
///
/// ```
/// use kevy_replicate::wire::{decode_frame, encode_frame};
///
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// let mut stream = encode_frame(0, &set);
/// stream.extend(encode_frame(1, &set));
/// let (first, used) = decode_frame(&stream)?;
/// let (second, _) = decode_frame(&stream[used..])?; // advance past the first
/// assert_eq!((first.offset, second.offset), (0, 1));
/// # Ok::<(), kevy_replicate::wire::WireError>(())
/// ```
pub fn decode_frame(buf: &[u8]) -> Result<(DecodedFrame, usize), WireError> {
    // Outer envelope: must be exactly `*2\r\n`.
    let after_env = parse_envelope_header(buf)?;
    // Offset line: `:<u64>\r\n`.
    let (offset, after_offset) = parse_offset_line(buf, after_env)?;
    // Inner argv: defer to kevy-resp's RESP2 multi-bulk parser.
    let inner = &buf[after_offset..];
    let mut argv = Argv::default();
    let consumed_inner = match parse_command_into(inner, &mut argv) {
        Ok(Some(n)) => n,
        Ok(None) => return Err(WireError::Truncated),
        Err(e) => return Err(WireError::BadPayload(e)),
    };
    Ok((DecodedFrame { offset, argv }, after_offset + consumed_inner))
}

/// Verify the outer `*2\r\n` header and return the cursor position just
/// after the trailing CRLF.
fn parse_envelope_header(buf: &[u8]) -> Result<usize, WireError> {
    // Need at least `*N\r\n` — minimum 4 bytes for `*2\r\n`.
    if buf.len() < 4 {
        return Err(WireError::Truncated);
    }
    if buf[0] != b'*' {
        return Err(WireError::BadEnvelope);
    }
    let eol = find_crlf(buf, 1).ok_or(WireError::Truncated)?;
    let count = parse_decimal(&buf[1..eol]).ok_or(WireError::BadEnvelope)?;
    if count != 2 {
        return Err(WireError::BadEnvelope);
    }
    Ok(eol + 2)
}

/// Parse `:<int>\r\n` starting at `start`; return `(offset, new_cursor)`.
fn parse_offset_line(buf: &[u8], start: usize) -> Result<(u64, usize), WireError> {
    if start >= buf.len() {
        return Err(WireError::Truncated);
    }
    if buf[start] != b':' {
        return Err(WireError::BadOffset);
    }
    let eol = find_crlf(buf, start + 1).ok_or(WireError::Truncated)?;
    let raw = &buf[start + 1..eol];
    // Allow a leading `-` so we can return NegativeOffset with the
    // value instead of a generic BadOffset for that specific case.
    let signed = parse_signed_decimal(raw).ok_or(WireError::BadOffset)?;
    if signed < 0 {
        return Err(WireError::NegativeOffset(signed));
    }
    Ok((signed as u64, eol + 2))
}

/// Find the next `\r\n` at or after `from`. Returns the index of the
/// `\r` byte. `None` = no CRLF in remaining buffer.
pub(crate) fn find_crlf(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < buf.len() {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse an unsigned decimal byte slice. Empty / non-digit = `None`.
pub(crate) fn parse_decimal(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(n)
}

/// Parse a signed decimal byte slice (`-` or `+` optional, then digits).
/// Empty / overflow / non-digit body = `None`.
fn parse_signed_decimal(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let (neg, digits) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let n = parse_decimal(digits)?;
    if neg {
        // i64::MIN handling: parse_decimal returns u64 so n can be up
        // to i64::MAX as u64 + 1, exactly i64::MIN when negated.
        if n > (i64::MAX as u64) + 1 {
            return None;
        }
        if n == (i64::MAX as u64) + 1 {
            return Some(i64::MIN);
        }
        Some(-(n as i64))
    } else {
        if n > i64::MAX as u64 {
            return None;
        }
        Some(n as i64)
    }
}

/// Append the base-10 representation of `n` to `out` without allocating
/// an intermediate string.
pub(crate) fn push_u64(out: &mut Vec<u8>, n: u64) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20]; // u64::MAX is 20 digits
    let mut i = tmp.len();
    let mut v = n;
    while v != 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&tmp[i..]);
}

/// Rough size estimate for pre-allocating the encoded frame buffer.
/// Generic over [`ArgvView`] so the borrowed and owned hot paths share
/// the same pre-allocation logic.
fn argv_byte_estimate_view<A: ArgvView + ?Sized>(argv: &A) -> usize {
    // 10 bytes of overhead per argument (`$N\r\n` header + trailing CRLF
    // worst-case) plus the raw argv bytes.
    let mut bytes = 0usize;
    for i in 0..argv.len() {
        bytes += argv[i].len();
    }
    bytes + argv.len() * 10
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
