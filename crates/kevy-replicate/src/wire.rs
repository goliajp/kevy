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

use kevy_resp::{Argv, ArgvView, ProtocolError, parse_command_into};

// Snapshot ship helpers live in [`crate::wire_snapshot`] (split out
// to keep this file under the 500-LOC project ceiling); re-export
// here so the canonical import path stays `kevy_replicate::wire::*`.
pub use crate::wire_snapshot::{
    SNAPSHOT_CHUNK_MAX, SNAPSHOT_LINE_MAX, SnapshotMarker, decode_snapshot_chunk,
    decode_snapshot_marker, encode_snapshot_begin, encode_snapshot_chunk, encode_snapshot_end,
};

/// Wire-layer error. Only [`WireError::Truncated`] is recoverable by
/// the caller (read more bytes and retry); the other variants signal
/// a corrupt or protocol-violating peer and call for dropping the
/// connection.
#[derive(Debug)]
pub enum WireError {
    /// Buffer ended before a complete frame; accumulate more bytes
    /// and call [`decode_frame`] again.
    Truncated,
    /// Outer envelope did not start with `*2\r\n` (the only legal
    /// envelope length in v1.18.0).
    BadEnvelope,
    /// Offset element did not parse as a RESP integer (`:N\r\n`).
    BadOffset,
    /// RESP integer parsed but is negative. Offsets are `u64`.
    NegativeOffset(i64),
    /// Inner multi-bulk argv was malformed at the RESP layer.
    BadPayload(ProtocolError),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "wire frame truncated"),
            Self::BadEnvelope => write!(f, "wire envelope not *2"),
            Self::BadOffset => write!(f, "wire offset element not RESP integer"),
            Self::NegativeOffset(n) => write!(f, "wire offset is negative: {n}"),
            Self::BadPayload(e) => write!(f, "wire inner payload malformed: {e:?}"),
        }
    }
}

impl std::error::Error for WireError {}

impl PartialEq for WireError {
    fn eq(&self, other: &Self) -> bool {
        // ProtocolError carries `&'static str` reasons; comparing
        // discriminants is enough for test assertions. Avoids
        // forcing PartialEq onto ProtocolError just for the test
        // surface here.
        core::mem::discriminant(self) == core::mem::discriminant(other)
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
pub fn encode_frame<A: ArgvView + ?Sized>(offset: u64, argv: &A) -> Vec<u8> {
    debug_assert!(
        offset <= i64::MAX as u64,
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
/// Returns `(offset, argv, used)` on success; `used` is the number of
/// bytes the frame consumed (advance the caller's read cursor by that
/// much). On [`WireError::Truncated`], the caller should read more
/// bytes and retry; any other error signals an unrecoverable peer
/// violation.
pub fn decode_frame(buf: &[u8]) -> Result<(u64, Argv, usize), WireError> {
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
    Ok((offset, argv, after_offset + consumed_inner))
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
mod tests {
    use super::*;

    fn argv_from(args: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for arg in args {
            a.push(arg);
        }
        a
    }

    #[test]
    fn roundtrip_simple_set() {
        let argv = argv_from(&[b"SET", b"foo", b"bar"]);
        let bytes = encode_frame(42, &argv);
        let (offset, decoded, used) = decode_frame(&bytes).expect("decode");
        assert_eq!(offset, 42);
        assert_eq!(decoded, argv);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn roundtrip_offset_zero_and_max() {
        // Offset is u64 in the API but wire envelope (RESP integer) caps
        // at i64::MAX. See docs/wire.md + encode_frame's doc comment.
        for offset in [0u64, 1, i64::MAX as u64] {
            let argv = argv_from(&[b"PING"]);
            let bytes = encode_frame(offset, &argv);
            let (back, _, _) = decode_frame(&bytes).expect("decode");
            assert_eq!(back, offset);
        }
    }

    #[test]
    #[cfg(debug_assertions)] // the trip wire is a debug_assert! — release builds skip the panic by design
    #[should_panic(expected = "exceeds i64::MAX")]
    fn encoding_offset_above_i64_max_panics_in_debug() {
        // Catches accidental over-encoding before the frame goes on the
        // wire. In release builds the assert is gone and the peer would
        // see a BadOffset; we test the debug-build trip wire here.
        let argv = argv_from(&[b"PING"]);
        let _ = encode_frame(u64::MAX, &argv);
    }

    #[test]
    fn roundtrip_argv_with_binary_and_empty_args() {
        let bin: Vec<u8> = (0u8..=255).collect();
        let argv = argv_from(&[b"HSET", b"key", b"field", &bin, b""]);
        let bytes = encode_frame(7, &argv);
        let (_, decoded, _) = decode_frame(&bytes).expect("decode");
        assert_eq!(decoded.len(), 5);
        assert_eq!(decoded.get(3), Some(bin.as_slice()));
        assert_eq!(decoded.get(4), Some(&b""[..]));
    }

    #[test]
    fn two_concatenated_frames_decode_in_order() {
        let a = encode_frame(1, &argv_from(&[b"SET", b"k", b"a"]));
        let b = encode_frame(2, &argv_from(&[b"DEL", b"k"]));
        let mut buf = a.clone();
        buf.extend_from_slice(&b);

        let (off1, argv1, used1) = decode_frame(&buf).expect("frame 1");
        assert_eq!(off1, 1);
        assert_eq!(argv1, argv_from(&[b"SET", b"k", b"a"]));
        assert_eq!(used1, a.len());

        let (off2, argv2, used2) = decode_frame(&buf[used1..]).expect("frame 2");
        assert_eq!(off2, 2);
        assert_eq!(argv2, argv_from(&[b"DEL", b"k"]));
        assert_eq!(used1 + used2, buf.len());
    }

    #[test]
    fn offsets_are_strictly_increasing_when_emitted_in_order() {
        let mut bytes = Vec::new();
        for o in 0u64..16 {
            bytes.extend(encode_frame(o, &argv_from(&[b"PING"])));
        }
        let mut pos = 0;
        let mut last: Option<u64> = None;
        while pos < bytes.len() {
            let (offset, _, used) = decode_frame(&bytes[pos..]).expect("decode");
            if let Some(prev) = last {
                assert!(offset > prev, "offset {offset} not > prev {prev}");
            }
            last = Some(offset);
            pos += used;
        }
        assert_eq!(last, Some(15));
        assert_eq!(pos, bytes.len());
    }

    #[test]
    fn truncated_envelope_is_truncated_not_bad() {
        // Empty.
        assert_eq!(decode_frame(&[]), Err(WireError::Truncated));
        // Just `*` no header end.
        assert_eq!(decode_frame(b"*"), Err(WireError::Truncated));
        // `*2\r\n` then nothing.
        assert_eq!(decode_frame(b"*2\r\n"), Err(WireError::Truncated));
        // Offset start with no CRLF.
        assert_eq!(decode_frame(b"*2\r\n:42"), Err(WireError::Truncated));
        // Header + offset but inner argv missing.
        assert_eq!(decode_frame(b"*2\r\n:42\r\n"), Err(WireError::Truncated));
        // Header + offset + partial inner array.
        assert_eq!(decode_frame(b"*2\r\n:42\r\n*1\r\n$3\r\nfo"), Err(WireError::Truncated));
    }

    #[test]
    fn wrong_envelope_count_rejected() {
        // *1 instead of *2.
        let bad = b"*1\r\n:42\r\n";
        assert!(matches!(decode_frame(bad), Err(WireError::BadEnvelope)));
        // *3 (future-extension shape) rejected on v1.18.0.
        let bad3 = b"*3\r\n:42\r\n*0\r\n:0\r\n";
        assert!(matches!(decode_frame(bad3), Err(WireError::BadEnvelope)));
    }

    #[test]
    fn non_array_envelope_rejected() {
        // Starts with `:` instead of `*`.
        let bad = b":42\r\n*1\r\n$4\r\nPING\r\n";
        assert!(matches!(decode_frame(bad), Err(WireError::BadEnvelope)));
    }

    #[test]
    fn offset_not_integer_rejected() {
        // Second element is a bulk string, not an integer.
        let bad = b"*2\r\n$2\r\n42\r\n*1\r\n$4\r\nPING\r\n";
        assert!(matches!(decode_frame(bad), Err(WireError::BadOffset)));
    }

    #[test]
    fn negative_offset_rejected_with_value() {
        let bad = b"*2\r\n:-7\r\n*1\r\n$4\r\nPING\r\n";
        match decode_frame(bad) {
            Err(WireError::NegativeOffset(n)) => assert_eq!(n, -7),
            other => panic!("expected NegativeOffset, got {other:?}"),
        }
    }

    #[test]
    fn malformed_inner_payload_surfaces_bad_payload() {
        // Outer envelope + offset OK, inner claims `*1` but follows with
        // an unknown type byte (`!`) — the inner parser rejects.
        let bad = b"*2\r\n:1\r\n*1\r\n!nope\r\n";
        assert!(matches!(decode_frame(bad), Err(WireError::BadPayload(_))));
    }

    #[test]
    fn offset_with_extra_digits_overflow_rejected() {
        // 21 nines — bigger than u64::MAX (20 digits). parse_decimal
        // returns None on the checked-multiply overflow, and parse_signed
        // returns None on top of that, so we see BadOffset.
        let mut bad = b"*2\r\n:".to_vec();
        bad.extend(std::iter::repeat_n(b'9', 21));
        bad.extend_from_slice(b"\r\n*1\r\n$4\r\nPING\r\n");
        assert!(matches!(decode_frame(&bad), Err(WireError::BadOffset)));
    }

    // Snapshot-wire tests (T1.22) live in `tests/wire_snapshot.rs`
    // as an integration test so this file stays under the 500-LOC
    // project ceiling. Only public API there.

    #[test]
    fn encoded_bytes_are_exactly_what_spec_says() {
        // Hand-spell the spec's example so any future refactor that
        // changes byte order trips this test.
        let argv = argv_from(&[b"SET", b"foo", b"bar"]);
        let bytes = encode_frame(99, &argv);
        let expected =
            b"*2\r\n:99\r\n*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        assert_eq!(bytes, expected);
    }
}
