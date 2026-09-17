//! Reply-side parser (client perspective): parse server responses into a
//! [`Reply`] enum. Mirror of the encoders in [`crate::reply_encode`] and
//! [`crate::reply_encode_resp3`].
//!
//! Speaks RESP2 (the seven legacy prefixes — `+`/`-`/`:`/`$`/`*`/`$-1`/`*-1`)
//! and the additive RESP3 set (`%` map, `~` set, `,` double, `#` boolean,
//! `=` verbatim string, `(` big number, `_` null, `>` push, `!` blob error,
//! `|` attributes). RESP2-only callers can still ignore the new [`Reply`]
//! variants — the parser only produces them when the server speaks RESP3.

use crate::error::ProtocolError;
use crate::request::{find_crlf, parse_int};

/// A parsed RESP reply (server → client) — the client-side counterpart of
/// the crate's `encode_*` functions (server-side encoders).
///
/// Variants prefixed with `Resp3:` in their doc are only ever produced by
/// a server speaking RESP3; an `HELLO 2` (or no `HELLO`) session sees the
/// RESP2 subset (`Simple` / `Error` / `Int` / `Bulk` / `Nil` / `Array`)
/// exclusively. Adding new variants is non-breaking: an exhaustive
/// `match` on `Reply` is forced to opt into RESP3 by listing each variant
/// (rust 2024 will not warn on missing arms only after `#[non_exhaustive]`
/// — which we deliberately omit so RESP2-only code stays compile-checked
/// for completeness).
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// `+OK`
    Simple(Vec<u8>),
    /// `-ERR ...`
    Error(Vec<u8>),
    /// `:42`
    Int(i64),
    /// `$5\r\nhello\r\n`
    Bulk(Vec<u8>),
    /// `$-1` or `*-1` — the RESP2 null sentinel; in RESP3 the dedicated
    /// [`Reply::Null`] (`_\r\n`) is used instead. Both round-trip here.
    Nil,
    /// `*N ...`
    Array(Vec<Reply>),
    /// **Resp3:** `%N\r\n<key1><value1>...<keyN><valueN>` — N pairs (the
    /// header count is the pair count, NOT the element count, so a map of
    /// 3 pairs is `%3` plus 6 sub-replies). Parsed/exposed as a Vec of
    /// pairs so duplicate keys + insertion order are preserved.
    Map(Vec<(Reply, Reply)>),
    /// **Resp3:** `~N\r\n<item1>...<itemN>` — set semantics on the wire;
    /// dedup is the application's job (RESP3 doesn't require it).
    Set(Vec<Reply>),
    /// **Resp3:** `,1.5\r\n` — double. `inf` / `-inf` / `nan` are valid
    /// payloads per the RESP3 spec and survive the round-trip.
    Double(f64),
    /// **Resp3:** `#t\r\n` / `#f\r\n` — boolean.
    Boolean(bool),
    /// **Resp3:** `=15\r\ntxt:Some bytes\r\n` — verbatim string carrying
    /// a 3-char format tag (`txt` / `mkd` / etc.) + raw bytes. The colon
    /// separator is part of the wire encoding but not part of `data`.
    Verbatim {
        /// 3-char format tag (e.g. `b"txt"` for plain text, `b"mkd"` for markdown).
        fmt: [u8; 3],
        /// Payload bytes following the `:` separator.
        data: Vec<u8>,
    },
    /// **Resp3:** `(170141183460469231731687303715884105727\r\n` — arbitrary-
    /// precision integer; carried as the raw digit bytes since we don't
    /// pull in a bignum crate (charter: zero deps).
    BigNumber(Vec<u8>),
    /// **Resp3:** `_\r\n` — true null. RESP2 falls back to [`Reply::Nil`].
    Null,
    /// **Resp3:** `>N\r\n...` — like [`Reply::Array`] but tagged as an
    /// out-of-band server-push frame (pub/sub messages in RESP3). The
    /// client must dispatch these separately from regular replies.
    Push(Vec<Reply>),
    /// **Resp3:** `!8\r\nERR ohno\r\n` — error carried as a length-prefixed
    /// bulk (handles errors containing CRLF that the simple-string `-`
    /// shape can't encode).
    BlobError(Vec<u8>),
}

/// Parse one RESP reply from the front of `buf`. Speaks RESP2 + RESP3.
///
/// - `Ok(Some((reply, consumed)))` — a complete reply.
/// - `Ok(None)` — need more bytes.
/// - `Err(_)` — malformed.
///
/// Attributes (`|N\r\n…<reply>`) are transparently consumed and
/// discarded — they decorate the *next* reply but the parser surfaces
/// only the underlying reply, matching what every RESP3 client library
/// does today. Exposing them is a future addition once a real consumer
/// (e.g. CLIENT TRACE) ships.
pub fn parse_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    parse_with(buf, &mut DoubleText(None))
}

/// [`parse_reply`], also returning the wire text of every double in the
/// reply, in the order a depth-first walk of the reply meets them.
///
/// [`Reply::Double`] carries the parsed `f64`, and a value does not say how
/// the server wrote it: `1e+300`, `1e300` and a 301-digit integer are one
/// number. A client that shows replies as the server sent them — a CLI
/// that must print what `redis-cli` prints — needs the bytes.
///
/// # Examples
///
/// ```
/// use kevy_resp::{Reply, parse_reply_keeping_double_text};
///
/// let wire = b"*2\r\n,1e+300\r\n,inf\r\n";
/// let (reply, used, texts) = parse_reply_keeping_double_text(wire)?.expect("a whole reply");
/// assert_eq!(reply, Reply::Array(vec![Reply::Double(1e300), Reply::Double(f64::INFINITY)]));
/// assert_eq!(used, wire.len());
/// assert_eq!(texts, [b"1e+300".to_vec(), b"inf".to_vec()]);
/// # Ok::<(), kevy_resp::ProtocolError>(())
/// ```
///
/// # Errors
///
/// [`ProtocolError`] when the bytes are not RESP, exactly as [`parse_reply`].
pub fn parse_reply_keeping_double_text(buf: &[u8]) -> Result<Option<TextedReply>, ProtocolError> {
    let mut texts = Vec::new();
    let parsed = parse_with(buf, &mut DoubleText(Some(&mut texts)))?;
    Ok(parsed.map(|(reply, used)| (reply, used, texts)))
}

/// A reply, the bytes it consumed, and the wire text of each double in it.
///
/// # Examples
///
/// ```
/// use kevy_resp::{Reply, TextedReply, parse_reply_keeping_double_text};
///
/// let parsed: Option<TextedReply> = parse_reply_keeping_double_text(b",1.5\r\n")?;
/// let (reply, used, texts) = parsed.expect("a whole reply");
/// assert_eq!((reply, used, texts), (Reply::Double(1.5), 6, vec![b"1.5".to_vec()]));
/// # Ok::<(), kevy_resp::ProtocolError>(())
/// ```
pub type TextedReply = (Reply, usize, Vec<Vec<u8>>);

/// Where double text goes when a caller asked for it; nowhere otherwise.
struct DoubleText<'a>(Option<&'a mut Vec<Vec<u8>>>);

fn parse_with(
    buf: &[u8],
    dt: &mut DoubleText<'_>,
) -> Result<Option<(Reply, usize)>, ProtocolError> {
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
        b'*' => parse_array_reply(buf, false, dt),
        // ── RESP3 additions ──────────────────────────────────────────
        b'%' => parse_map_reply(buf, dt),
        b'~' => parse_set_reply(buf, dt),
        b',' => parse_double_reply(buf, dt),
        b'#' => parse_boolean_reply(buf),
        b'=' => parse_verbatim_reply(buf),
        b'(' => match reply_line(buf) {
            None => Ok(None),
            Some((b, used)) => Ok(Some((Reply::BigNumber(b.to_vec()), used))),
        },
        b'_' => parse_null_reply(buf),
        b'>' => parse_array_reply(buf, true, dt),
        b'!' => parse_blob_error_reply(buf),
        b'|' => parse_attributed_reply(buf, dt),
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
    payload_end(buf, data_end, "bulk payload not followed by CRLF")?;
    Ok(Some((Reply::Bulk(buf[data_start..data_end].to_vec()), data_end + 2)))
}

/// Shared parser for `*` (array, RESP2) and `>` (push, RESP3) — both
/// are length-prefixed sequences of replies. `push=true` wraps the
/// result in `Reply::Push`, otherwise `Reply::Array` (or `Reply::Nil`
/// for the RESP2 `*-1` shape, which RESP3 push frames never emit).
fn parse_array_reply(
    buf: &[u8],
    push: bool,
    dt: &mut DoubleText<'_>,
) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let count = parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad array length"))?;
    if count < 0 {
        if push {
            return Err(ProtocolError::Malformed("push frame cannot be null"));
        }
        return Ok(Some((Reply::Nil, hdr_end + 2)));
    }
    let mut pos = hdr_end + 2;
    // Cap initial capacity by remaining buffer bytes — an attacker-controlled
    // `*999999999999\r\n` header would otherwise panic via `Vec::with_capacity`'s
    // capacity overflow. Each item costs ≥ 1 byte (a CRLF for Nil/Int/Simple),
    // so a real array of N items needs ≥ N bytes left. Push will grow the vec
    // amortized if the genuine count is higher but bytes are present. Found by
    // cargo-fuzz (crash-4c4ee6777903d009f93289eb428b3b371d027137).
    let cap = (count as usize).min(buf.len().saturating_sub(pos));
    let mut items = Vec::with_capacity(cap);
    for _ in 0..count {
        match parse_with(&buf[pos..], dt)? {
            None => return Ok(None),
            Some((r, used)) => {
                items.push(r);
                pos += used;
            }
        }
    }
    let reply = if push { Reply::Push(items) } else { Reply::Array(items) };
    Ok(Some((reply, pos)))
}

/// `%N\r\n` followed by 2N sub-replies (N key/value pairs).
fn parse_map_reply(
    buf: &[u8],
    dt: &mut DoubleText<'_>,
) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let count = parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad map length"))?;
    if count < 0 {
        return Err(ProtocolError::Malformed("map length cannot be negative"));
    }
    let mut pos = hdr_end + 2;
    // Same fuzz-driven cap as parse_array_reply — each pair costs ≥ 2 bytes.
    let cap = (count as usize).min(buf.len().saturating_sub(pos) / 2);
    let mut pairs: Vec<(Reply, Reply)> = Vec::with_capacity(cap);
    for _ in 0..count {
        let Some((k, used_k)) = parse_with(&buf[pos..], dt)? else {
            return Ok(None);
        };
        pos += used_k;
        let Some((v, used_v)) = parse_with(&buf[pos..], dt)? else {
            return Ok(None);
        };
        pos += used_v;
        pairs.push((k, v));
    }
    Ok(Some((Reply::Map(pairs), pos)))
}

/// `~N\r\n` followed by N sub-replies — set on the wire, no dedup.
fn parse_set_reply(
    buf: &[u8],
    dt: &mut DoubleText<'_>,
) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let count = parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad set length"))?;
    if count < 0 {
        return Err(ProtocolError::Malformed("set length cannot be negative"));
    }
    let mut pos = hdr_end + 2;
    let cap = (count as usize).min(buf.len().saturating_sub(pos));
    let mut items = Vec::with_capacity(cap);
    for _ in 0..count {
        match parse_with(&buf[pos..], dt)? {
            None => return Ok(None),
            Some((r, used)) => {
                items.push(r);
                pos += used;
            }
        }
    }
    Ok(Some((Reply::Set(items), pos)))
}

/// `,N\r\n` — double. RESP3 spec carries `inf` / `-inf` / `nan` as
/// literal byte strings; `f64::from_str` already handles all three.
fn parse_double_reply(
    buf: &[u8],
    dt: &mut DoubleText<'_>,
) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some((bytes, used)) = reply_line(buf) else {
        return Ok(None);
    };
    let s = std::str::from_utf8(bytes).map_err(|_| ProtocolError::Malformed("bad double utf8"))?;
    let v: f64 = s.parse().map_err(|_| ProtocolError::Malformed("bad double"))?;
    if let Some(texts) = dt.0.as_mut() {
        texts.push(bytes.to_vec());
    }
    Ok(Some((Reply::Double(v), used)))
}

/// `#t\r\n` / `#f\r\n` — boolean. Any other payload is malformed.
fn parse_boolean_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some((bytes, used)) = reply_line(buf) else {
        return Ok(None);
    };
    let v = match bytes {
        b"t" => true,
        b"f" => false,
        _ => return Err(ProtocolError::Malformed("bad boolean payload")),
    };
    Ok(Some((Reply::Boolean(v), used)))
}

/// `=N\r\n<fmt>:<data>\r\n` — verbatim string. The 3-char `fmt` tag +
/// `:` separator are inside the N-byte body.
fn parse_verbatim_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let len = parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad verbatim length"))?;
    if len < 4 {
        return Err(ProtocolError::Malformed("verbatim length < 4 (fmt + ':')"));
    }
    let data_start = hdr_end + 2;
    let data_end = data_start + len as usize;
    if buf.len() < data_end + 2 {
        return Ok(None);
    }
    payload_end(buf, data_end, "verbatim payload not followed by CRLF")?;
    let body = &buf[data_start..data_end];
    if body[3] != b':' {
        return Err(ProtocolError::Malformed("verbatim missing fmt:data separator"));
    }
    let mut fmt = [0u8; 3];
    fmt.copy_from_slice(&body[..3]);
    let data = body[4..].to_vec();
    Ok(Some((Reply::Verbatim { fmt, data }, data_end + 2)))
}

/// `_\r\n` — RESP3 true null (5 bytes counting the `_` and CRLF).
fn parse_null_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    if buf.len() < 3 {
        return Ok(None);
    }
    if &buf[..3] != b"_\r\n" {
        return Err(ProtocolError::Malformed("bad null payload"));
    }
    Ok(Some((Reply::Null, 3)))
}

/// `!N\r\n<error>\r\n` — length-prefixed error (carries CRLF safely).
fn parse_blob_error_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let len =
        parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad blob error length"))?;
    if len < 0 {
        return Err(ProtocolError::Malformed("blob error length cannot be negative"));
    }
    let data_start = hdr_end + 2;
    let data_end = data_start + len as usize;
    if buf.len() < data_end + 2 {
        return Ok(None);
    }
    payload_end(buf, data_end, "blob error payload not followed by CRLF")?;
    Ok(Some((Reply::BlobError(buf[data_start..data_end].to_vec()), data_end + 2)))
}

/// A length-prefixed payload ends in CRLF. A wrong length prefix would
/// otherwise be read as a shorter payload and the rest of the stream
/// parsed from the middle of a value — silently, a reply out of step with
/// its request. The request parser has always refused this.
fn payload_end(buf: &[u8], data_end: usize, what: &'static str) -> Result<(), ProtocolError> {
    if &buf[data_end..data_end + 2] == b"\r\n" {
        Ok(())
    } else {
        Err(ProtocolError::Malformed(what))
    }
}

/// `|N\r\n<map of N pairs><reply>` — attributes decorate the next reply.
/// We parse the attribute map then transparently return the decorated
/// reply, mirroring what RESP3 client libraries do today. The attributes
/// themselves are dropped (see [`parse_reply`] docs).
fn parse_attributed_reply(
    buf: &[u8],
    dt: &mut DoubleText<'_>,
) -> Result<Option<(Reply, usize)>, ProtocolError> {
    // Re-use the map parser but throw away the result; then parse the
    // actual reply that follows.
    let Some((_attrs, used_attrs)) = parse_map_reply(buf, dt)? else {
        return Ok(None);
    };
    match parse_with(&buf[used_attrs..], dt)? {
        None => Ok(None),
        Some((r, used)) => Ok(Some((r, used_attrs + used))),
    }
}

#[cfg(test)]
#[path = "reply_parse_tests.rs"]
mod tests;
