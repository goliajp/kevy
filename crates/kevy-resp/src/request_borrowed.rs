//! Borrowed-buffer request parser: zero-copy alternative to [`parse_command`].
//!
//! Records each arg as a `(start, end)` range into the caller's input buffer
//! rather than copying its bytes. Pairs with [`crate::ArgvBorrowed`].

use crate::argv_borrowed::ArgvBorrowed;
use crate::error::ProtocolError;
use crate::request::{find_crlf, parse_int, validate_multibulk_frame};

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
pub fn parse_command_borrowed(
    buf: &[u8],
) -> Result<Option<(ArgvBorrowed<'_>, usize)>, ProtocolError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] == b'*' {
        parse_multibulk_borrowed(buf)
    } else {
        parse_inline_borrowed(buf)
    }
}

fn parse_inline_borrowed(
    buf: &[u8],
) -> Result<Option<(ArgvBorrowed<'_>, usize)>, ProtocolError> {
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
    let start = hdr_end + 2;

    let (end_pos, _total) = match validate_multibulk_frame(buf, start, count)? {
        Some(t) => t,
        None => return Ok(None),
    };

    let mut argv = ArgvBorrowed::with_capacity(buf, count);
    let mut p = start;
    for _ in 0..count {
        let len_end = find_crlf(buf, p + 1).expect("validated in pass 1");
        let len = parse_int(&buf[p + 1..len_end]).expect("validated in pass 1") as usize;
        let data_start = len_end + 2;
        argv.push_range(data_start, data_start + len);
        p = data_start + len + 2;
    }
    Ok(Some((argv, end_pos)))
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
            assert!(
                p >= base && p + slice.len() <= end,
                "arg {i} not borrowed from buf"
            );
        }
    }

    #[test]
    fn borrowed_incomplete_returns_none() {
        assert!(parse_command_borrowed(b"*1\r\n$4\r\nPI").unwrap().is_none());
        assert!(
            parse_command_borrowed(b"*2\r\n$4\r\nECHO\r\n")
                .unwrap()
                .is_none()
        );
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
        assert_eq!(
            argv,
            vec![b"ECHO".to_vec(), b"hi".to_vec(), b"there".to_vec()]
        );
    }

    #[test]
    fn borrowed_malformed_errors() {
        assert!(parse_command_borrowed(b"*1\r\n+OK\r\n").is_err());
        assert!(parse_command_borrowed(b"*x\r\n").is_err());
    }

    #[test]
    fn borrowed_null_array_yields_empty_argv() {
        let frame = b"*-1\r\n";
        let (argv, used) = parse_command_borrowed(frame).unwrap().unwrap();
        assert!(argv.is_empty());
        assert_eq!(used, frame.len());
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
            assert_eq!(owned_used, b_used, "consumed mismatch for {:?}", frame);
            let materialised = borrowed.into_owned();
            assert_eq!(owned, materialised, "argv mismatch for {:?}", frame);
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
