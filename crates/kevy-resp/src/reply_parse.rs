//! Reply-side parser (client perspective): parse server responses into a
//! [`Reply`] enum. Mirror of the encoders in [`crate::reply_encode`].

use crate::error::ProtocolError;
use crate::request::{find_crlf, parse_int};

/// A parsed RESP reply (server → client) — the client-side counterpart of the
/// `encode_*` functions in [`crate::reply_encode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `+OK`
    Simple(Vec<u8>),
    /// `-ERR ...`
    Error(Vec<u8>),
    /// `:42`
    Int(i64),
    /// `$5\r\nhello\r\n`
    Bulk(Vec<u8>),
    /// `$-1` or `*-1`
    Nil,
    /// `*N ...`
    Array(Vec<Reply>),
}

/// Parse one RESP reply from the front of `buf`.
///
/// - `Ok(Some((reply, consumed)))` — a complete reply.
/// - `Ok(None)` — need more bytes.
/// - `Err(_)` — malformed.
pub fn parse_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(&tag) = buf.first() else {
        return Ok(None);
    };
    match tag {
        b'+' => Ok(reply_line(buf).map(|(b, used)| (Reply::Simple(b.to_vec()), used))),
        b'-' => Ok(reply_line(buf).map(|(b, used)| (Reply::Error(b.to_vec()), used))),
        b':' => match reply_line(buf) {
            None => Ok(None),
            Some((b, used)) => {
                let n = parse_int(b).ok_or(ProtocolError::Malformed("bad integer reply"))?;
                Ok(Some((Reply::Int(n), used)))
            }
        },
        b'$' => parse_bulk_reply(buf),
        b'*' => parse_array_reply(buf),
        _ => Err(ProtocolError::Malformed("unknown reply type")),
    }
}

/// The CRLF-terminated payload after the type byte, plus bytes consumed.
fn reply_line(buf: &[u8]) -> Option<(&[u8], usize)> {
    find_crlf(buf, 1).map(|eol| (&buf[1..eol], eol + 2))
}

fn parse_bulk_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let len = parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad bulk length"))?;
    if len < 0 {
        return Ok(Some((Reply::Nil, hdr_end + 2)));
    }
    let data_start = hdr_end + 2;
    let data_end = data_start + len as usize;
    if buf.len() < data_end + 2 {
        return Ok(None);
    }
    Ok(Some((
        Reply::Bulk(buf[data_start..data_end].to_vec()),
        data_end + 2,
    )))
}

fn parse_array_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let count = parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad array length"))?;
    if count < 0 {
        return Ok(Some((Reply::Nil, hdr_end + 2)));
    }
    let mut pos = hdr_end + 2;
    // Cap initial capacity by remaining buffer bytes — an attacker-controlled
    // `*999999999999\r\n` header would otherwise panic via `Vec::with_capacity`'s
    // capacity overflow. Each item costs ≥ 1 byte (a CRLF for Nil/Int/Simple),
    // so a real array of N items needs ≥ N bytes left. Push will grow the vec
    // amortized if the genuine count is higher but bytes are present. Found by
    // cargo-fuzz against crash-4c4ee6777903d009f93289eb428b3b371d027137 during
    // STONE-AUDIT Phase A #4 (2026-05-26).
    let cap = (count as usize).min(buf.len().saturating_sub(pos));
    let mut items = Vec::with_capacity(cap);
    for _ in 0..count {
        match parse_reply(&buf[pos..])? {
            None => return Ok(None),
            Some((r, used)) => {
                items.push(r);
                pos += used;
            }
        }
    }
    Ok(Some((Reply::Array(items), pos)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_replies() {
        let r = |b: &[u8]| parse_reply(b).unwrap().unwrap().0;
        assert_eq!(r(b"+OK\r\n"), Reply::Simple(b"OK".to_vec()));
        assert_eq!(r(b"-ERR bad\r\n"), Reply::Error(b"ERR bad".to_vec()));
        assert_eq!(r(b":42\r\n"), Reply::Int(42));
        assert_eq!(r(b"$5\r\nhello\r\n"), Reply::Bulk(b"hello".to_vec()));
        assert_eq!(r(b"$-1\r\n"), Reply::Nil);
        assert_eq!(r(b"*-1\r\n"), Reply::Nil);

        let (arr, used) = parse_reply(b"*2\r\n:1\r\n$2\r\nhi\r\n").unwrap().unwrap();
        assert_eq!(
            arr,
            Reply::Array(vec![Reply::Int(1), Reply::Bulk(b"hi".to_vec())])
        );
        assert_eq!(used, 16);

        // Incomplete replies ask for more bytes.
        assert_eq!(parse_reply(b"$5\r\nhel").unwrap(), None);
        assert_eq!(parse_reply(b"*2\r\n:1\r\n").unwrap(), None);
        assert!(parse_reply(b"!nope\r\n").is_err());
    }
}
