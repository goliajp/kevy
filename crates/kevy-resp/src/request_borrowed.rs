//! Borrowed-buffer request parser: zero-copy alternative to [`parse_command`].
//!
//! Records each arg as a `(start, end)` range into the caller's input buffer
//! rather than copying its bytes. Pairs with [`crate::ArgvBorrowed`].

use crate::argv_borrowed::ArgvBorrowed;
use crate::error::ProtocolError;
use crate::request::{MAX_MULTIBULK_LEN, find_crlf, parse_bulk_len, parse_int};

/// Parse one command from the front of `buf`, recording each arg as a
/// `(start, end)` range into `buf` rather than copying its bytes.
///
/// The returned [`ArgvBorrowed`] is a zero-copy view: `get(i)` slices directly
/// into `buf`. Use this on the local single-shard hot path; call
/// [`ArgvBorrowed::into_owned`] before storing an argv past the buffer's
/// lifetime (cross-shard dispatch, MULTI queue, AOF logging).
///
/// Return shape matches [`crate::parse_command`]: `Ok(Some((argv, consumed)))`,
/// `Ok(None)` if more bytes are needed, `Err` on malformed input.
#[inline]
pub fn parse_command_borrowed(
    buf: &[u8],
) -> Result<Option<(ArgvBorrowed<'_>, usize)>, ProtocolError> {
    // Empty parses (a bare CRLF inline line, `*-1`, `*0`) are consumed
    // SILENTLY and parsing continues — Redis semantics. redis-cli
    // --pipe ends every stream with "\r\nECHO <magic>": answering the
    // bare CRLF with an error makes every pipe import report
    // "errors: 1" (the migrationgate discovery).
    let mut base = 0usize;
    loop {
        let rest = &buf[base..];
        if rest.is_empty() {
            return Ok(None);
        }
        let parsed = if rest[0] == b'*' {
            parse_multibulk_borrowed(rest)?
        } else {
            parse_inline_borrowed(rest)?
        };
        match parsed {
            None => return Ok(None),
            Some((argv, used)) if argv.is_empty() => base += used,
            Some((argv, used)) => return Ok(Some((argv, base + used))),
        }
    }
}

// See note on `parse_inline_into`: signature symmetry with
// `parse_multibulk_borrowed` is the point; the inline path itself can't fail.
#[allow(clippy::unnecessary_wraps)]
fn parse_inline_borrowed(buf: &[u8]) -> Result<Option<(ArgvBorrowed<'_>, usize)>, ProtocolError> {
    let Some(eol) = find_crlf(buf, 0) else {
        return Ok(None);
    };
    let mut argv = ArgvBorrowed::new(buf);
    let line = &buf[..eol];
    let mut i = 0;
    while i < line.len() {
        if line[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < line.len() && !line[i].is_ascii_whitespace() {
            i += 1;
        }
        argv.push_range(start, i);
    }
    Ok(Some((argv, eol + 2)))
}

/// Single-pass multibulk parse: each arg's header is validated and its
/// `(start, end)` range recorded in the same walk. The old two-pass shape
/// (`validate_multibulk_frame`, then re-scan every header to record the
/// ranges) re-paid `find_crlf` + `parse_int` per arg — measurably ~half
/// the whole parse cost at the 8-shard bench corner. On `Ok(None)` /
/// `Err` the partially-built argv is simply dropped (a range vec; no
/// bytes were copied).
#[inline]
fn parse_multibulk_borrowed(
    buf: &[u8],
) -> Result<Option<(ArgvBorrowed<'_>, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let count =
        parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad multibulk count"))?;
    if count < 0 {
        return Ok(Some((ArgvBorrowed::new(buf), hdr_end + 2)));
    }
    let count = count as usize;
    // Reject an absurd declared count BEFORE it reaches
    // `with_capacity` — the single-pass borrowed parser reserves the
    // range table up front, so an unchecked count is a remote,
    // single-packet alloc-abort (the owned parser is gated the same
    // way in `parse_multibulk_into`).
    if count > MAX_MULTIBULK_LEN {
        return Err(ProtocolError::Malformed("multibulk count exceeds limit"));
    }

    let mut argv = ArgvBorrowed::with_capacity(buf, count);
    let mut p = hdr_end + 2;
    for _ in 0..count {
        match buf.get(p) {
            None => return Ok(None),
            Some(b'$') => {}
            Some(_) => return Err(ProtocolError::Malformed("expected bulk string")),
        }
        let Some((len, data_start)) = parse_bulk_len(buf, p)? else {
            return Ok(None);
        };
        let data_end = data_start + len;
        if buf.len() < data_end + 2 {
            return Ok(None);
        }
        if &buf[data_end..data_end + 2] != b"\r\n" {
            return Err(ProtocolError::Malformed("bulk string not CRLF-terminated"));
        }
        argv.push_range(data_start, data_end);
        p = data_end + 2;
    }
    Ok(Some((argv, p)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{encode_command, parse_command};

    #[test]
    fn borrowed_multibulk_ping() {
        let frame = b"*1\r\n$4\r\nPING\r\n";
        let (argv, used) = parse_command_borrowed(frame).unwrap().unwrap();
        assert_eq!(argv.len(), 1);
        assert_eq!(argv.first(), Some(b"PING" as &[u8]));
        assert_eq!(used, frame.len());
        // The arg slice points back into the original buffer (zero copy).
        assert_eq!(argv.get(0).unwrap().as_ptr(), frame[8..].as_ptr());
    }

    #[test]
    fn borrowed_multibulk_echo_zero_copy() {
        let frame = b"*2\r\n$4\r\nECHO\r\n$5\r\nhello\r\n";
        let (argv, used) = parse_command_borrowed(frame).unwrap().unwrap();
        assert_eq!(argv, vec![b"ECHO".to_vec(), b"hello".to_vec()]);
        assert_eq!(used, frame.len());
        // Each arg slice's pointer falls inside the input buffer — proves
        // the borrowed parser never copied the bytes elsewhere.
        let base = frame.as_ptr() as usize;
        let end = base + frame.len();
        for i in 0..argv.len() {
            let slice = argv.get(i).unwrap();
            let p = slice.as_ptr() as usize;
            assert!(p >= base && p + slice.len() <= end, "arg {i} not borrowed from buf");
        }
    }

    #[test]
    fn borrowed_incomplete_returns_none() {
        assert!(parse_command_borrowed(b"*1\r\n$4\r\nPI").unwrap().is_none());
        assert!(parse_command_borrowed(b"*2\r\n$4\r\nECHO\r\n").unwrap().is_none());
        assert!(parse_command_borrowed(b"").unwrap().is_none());
    }

    #[test]
    fn borrowed_inline_command() {
        let frame = b"PING\r\n";
        let (argv, used) = parse_command_borrowed(frame).unwrap().unwrap();
        assert_eq!(argv, vec![b"PING".to_vec()]);
        assert_eq!(used, frame.len());

        let frame = b"ECHO  hi there\r\n";
        let (argv, _) = parse_command_borrowed(frame).unwrap().unwrap();
        assert_eq!(argv, vec![b"ECHO".to_vec(), b"hi".to_vec(), b"there".to_vec()]);
    }

    #[test]
    fn borrowed_malformed_errors() {
        assert!(parse_command_borrowed(b"*1\r\n+OK\r\n").is_err());
        assert!(parse_command_borrowed(b"*x\r\n").is_err());
        // Bulk-header malformations through the fused single-pass walk.
        assert!(parse_command_borrowed(b"*1\r\n$x\r\n").is_err());
        assert!(parse_command_borrowed(b"*1\r\n$\r\n").is_err());
        assert!(parse_command_borrowed(b"*1\r\n$-1\r\n").is_err());
        assert!(parse_command_borrowed(b"*1\r\n$3\rXabc\r\n").is_err());
        // 20 nines overflows i64 → malformed, not a hang.
        assert!(parse_command_borrowed(b"*1\r\n$99999999999999999999\r\n").is_err());
        // Bulk data not CRLF-terminated.
        assert!(parse_command_borrowed(b"*1\r\n$3\r\nabcXX").is_err());
    }

    #[test]
    fn borrowed_incomplete_at_every_prefix_returns_none() {
        // The single-pass parser must report Ok(None) — never Err, never
        // Some — for every strict prefix of a valid frame (a split TCP
        // read can land at any byte).
        let frame = b"*3\r\n$3\r\nSET\r\n$16\r\nkey:000000000001\r\n$3\r\nxxx\r\n";
        for cut in 0..frame.len() {
            let r = parse_command_borrowed(&frame[..cut]);
            assert!(
                matches!(r, Ok(None)),
                "prefix len {cut} gave {:?}",
                r.map(|o| o.map(|(_, used)| used))
            );
        }
        let (argv, used) = parse_command_borrowed(frame).unwrap().unwrap();
        assert_eq!(used, frame.len());
        assert_eq!(argv.len(), 3);
        assert_eq!(argv.get(1), Some(b"key:000000000001" as &[u8]));
    }

    #[test]
    fn borrowed_plus_sign_bulk_len_matches_parse_int_semantics() {
        // parse_int accepts a leading '+'; the fused header parser keeps
        // that acceptance so the two parsers agree on every frame.
        let frame = b"*1\r\n$+4\r\nPING\r\n";
        let (argv, used) = parse_command_borrowed(frame).unwrap().unwrap();
        assert_eq!(argv, vec![b"PING".to_vec()]);
        assert_eq!(used, frame.len());
    }

    #[test]
    fn empty_parses_are_consumed_silently() {
        // Redis semantics: a null array, an empty array and a bare CRLF
        // inline line all vanish — the NEXT real command parses, and
        // `consumed` covers the skipped bytes too.
        for prefix in [b"*-1\r\n" as &[u8], b"*0\r\n", b"\r\n", b"\r\n\r\n"] {
            let mut stream = prefix.to_vec();
            stream.extend_from_slice(b"*1\r\n$4\r\nPING\r\n");
            let (argv, used) = parse_command_borrowed(&stream).unwrap().unwrap();
            assert_eq!(argv, vec![b"PING".to_vec()], "prefix {prefix:?}");
            assert_eq!(used, stream.len(), "prefix {prefix:?}");
        }
        // Nothing but empties → need more bytes, not a command.
        assert!(parse_command_borrowed(b"*-1\r\n").unwrap().is_none());
        // The redis-cli --pipe trailer shape end to end.
        let pipe_tail = b"\r\n*2\r\n$4\r\nECHO\r\n$3\r\nxyz\r\n";
        let (argv, used) = parse_command_borrowed(pipe_tail).unwrap().unwrap();
        assert_eq!(argv.first(), Some(b"ECHO" as &[u8]));
        assert_eq!(used, pipe_tail.len());
    }

    #[test]
    fn borrowed_into_owned_matches_parse_command() {
        // For every well-formed frame, parse_command_borrowed(buf).into_owned()
        // must equal parse_command(buf).0 — the materialised argv.
        let frames: &[&[u8]] = &[
            b"*1\r\n$4\r\nPING\r\n",
            b"*2\r\n$4\r\nECHO\r\n$5\r\nhello\r\n",
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nvalue\r\n",
            b"PING\r\n",
            b"ECHO  hi there\r\n",
        ];
        for frame in frames {
            let (owned, owned_used) = parse_command(frame).unwrap().unwrap();
            let (borrowed, b_used) = parse_command_borrowed(frame).unwrap().unwrap();
            assert_eq!(owned_used, b_used, "consumed mismatch for {frame:?}");
            let materialised = borrowed.into_owned();
            assert_eq!(owned, materialised, "argv mismatch for {frame:?}");
        }
    }

    #[test]
    fn borrowed_round_trip_command() {
        let mut buf = Vec::new();
        encode_command(&mut buf, &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
        let (argv, used) = parse_command_borrowed(&buf).unwrap().unwrap();
        assert_eq!(argv, vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
        assert_eq!(used, buf.len());
    }

    #[test]
    fn borrowed_handles_split_buffer_after_consumed() {
        // After consuming one frame, the next call sees only the remainder —
        // the borrow scopes don't leak across calls.
        let mut stream = Vec::new();
        encode_command(&mut stream, &[b"PING".to_vec()]);
        encode_command(&mut stream, &[b"ECHO".to_vec(), b"hi".to_vec()]);
        let (a, used) = parse_command_borrowed(&stream).unwrap().unwrap();
        assert_eq!(a, vec![b"PING".to_vec()]);
        let (b, used2) = parse_command_borrowed(&stream[used..]).unwrap().unwrap();
        assert_eq!(b, vec![b"ECHO".to_vec(), b"hi".to_vec()]);
        assert_eq!(used + used2, stream.len());
    }
}
