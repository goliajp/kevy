//! RESP3 reply encoders — the additive prefixes that ride on top of the
//! RESP2 wire (`+ - : $ * $-1 *-1`). Sibling of [`crate::reply_encode`]:
//! everything here is `out: &mut Vec<u8>` + zero-alloc-past-initial-reserve,
//! same shape as the RESP2 encoders, so a dispatch path can pick a proto
//! per connection without changing its calling convention.
//!
//! Wire format reference: <https://github.com/antirez/RESP3/blob/master/spec.md>
//!
//! The two big-ticket helpers are the **header** encoders
//! ([`encode_map_header`] / [`encode_set_header`] / [`encode_push_header`]).
//! Like [`encode_array_len`] in RESP2, they emit only the count prefix —
//! the caller follows up with the right number of sub-replies (2N for maps,
//! N for sets / push). This keeps the encoders alloc-free and matches how
//! dispatch already streams replies into the conn's output buffer.

/// `%<count>\r\n` — a map header. Follow with `count` × 2 sub-replies
/// (key₁ value₁ key₂ value₂ …). The count is the **pair** count, not
/// the element count.
///
/// Used for replies that RESP2 ships as `*2N` array-of-pairs — `HGETALL`,
/// `CONFIG GET`, `XINFO STREAM`. The map header is a single byte plus the
/// count digits; vs RESP2's `*2N` it saves zero header bytes but the
/// payload typically saves 4 B per pair by allowing simple-string keys.
pub fn encode_map_header(out: &mut Vec<u8>, count: i64) {
    out.push(b'%');
    push_int(out, count);
    out.extend_from_slice(b"\r\n");
}

/// `~<count>\r\n` — a set header. Follow with `count` sub-replies; the
/// receiving client treats them as a set (dedup is its job; the wire
/// doesn't require it).
///
/// Used for `SMEMBERS` / `SINTER` / `SUNION` / `SDIFF` / `SRANDMEMBER COUNT`.
pub fn encode_set_header(out: &mut Vec<u8>, count: i64) {
    out.push(b'~');
    push_int(out, count);
    out.extend_from_slice(b"\r\n");
}

/// `><count>\r\n` — an out-of-band push frame header. Follow with
/// `count` sub-replies. The RESP3 client demultiplexes push frames from
/// regular replies, so this is what `PUBLISH` / pattern-subscribe
/// delivery uses when the consumer speaks RESP3.
pub fn encode_push_header(out: &mut Vec<u8>, count: i64) {
    out.push(b'>');
    push_int(out, count);
    out.extend_from_slice(b"\r\n");
}

/// `,<value>\r\n` — a double. `inf` / `-inf` / `nan` are valid wire
/// payloads per spec; we forward Rust's standard float formatting which
/// emits exactly those tokens.
///
/// Vs RESP2's `$<len>\r\n<digits>\r\n` shape this saves ~6 B per value
/// (no length prefix, no trailing CRLF after the digits — the digits
/// ARE the CRLF-terminated line). Worth it on `ZSCORE` flood or
/// `ZRANGE WITHSCORES`.
pub fn encode_double(out: &mut Vec<u8>, v: f64) {
    out.push(b',');
    if v.is_nan() {
        out.extend_from_slice(b"nan");
    } else if v.is_infinite() {
        out.extend_from_slice(if v > 0.0 { b"inf" } else { b"-inf" });
    } else {
        // Match the wire shape RESP3 clients expect: an integer-valued
        // double serialises without a decimal point ("3" not "3.0"),
        // matching what the parse_double_reply round-trip expects.
        if v == v.trunc() && v.abs() < 1e17 {
            push_int(out, v as i64);
        } else {
            // Rust's default `{}` for f64 emits a shortest round-trippable
            // representation — same shape Redis emits for ZSCORE. Format
            // into a stack buffer (no heap alloc) then extend.
            use std::io::Write as _;
            let _ = write!(out, "{v}");
        }
    }
    out.extend_from_slice(b"\r\n");
}

/// `#t\r\n` / `#f\r\n` — boolean.
pub fn encode_boolean(out: &mut Vec<u8>, v: bool) {
    out.extend_from_slice(if v { b"#t\r\n" } else { b"#f\r\n" });
}

/// `_\r\n` — RESP3 true null. RESP2 fallback is the existing
/// [`crate::encode_null_bulk`] (`$-1\r\n`).
pub fn encode_null(out: &mut Vec<u8>) {
    out.extend_from_slice(b"_\r\n");
}

/// `(<digits>\r\n` — arbitrary-precision integer carried as its string
/// representation. We don't ship a bignum type (charter: zero deps), so
/// the caller hands in pre-formatted digit bytes.
pub fn encode_big_number(out: &mut Vec<u8>, digits: &[u8]) {
    out.reserve(digits.len() + 4);
    out.push(b'(');
    out.extend_from_slice(digits);
    out.extend_from_slice(b"\r\n");
}

/// `=<len>\r\n<fmt>:<data>\r\n` — verbatim string. `fmt` MUST be a
/// 3-byte format tag (`b"txt"`, `b"mkd"`, `b"raw"`, …); `data` is the
/// payload after the `:` separator. The wire `len` covers the 3-byte
/// fmt + `:` + payload (so `len = 4 + data.len()`).
///
/// Used for `CLIENT INFO` / `DEBUG OBJECT` style replies where a RESP3
/// client wants to know "this is markdown, render it as markdown" but
/// a RESP2 client still gets the raw bytes.
pub fn encode_verbatim(out: &mut Vec<u8>, fmt: [u8; 3], data: &[u8]) {
    let total_len = 4 + data.len();
    out.reserve(total_len + 16);
    out.push(b'=');
    push_int(out, total_len as i64);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&fmt);
    out.push(b':');
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
}

/// `!<len>\r\n<error>\r\n` — length-prefixed error. Use when the error
/// payload contains CRLF (the simple `-...` shape can't encode it).
pub fn encode_blob_error(out: &mut Vec<u8>, msg: &[u8]) {
    out.reserve(msg.len() + 16);
    out.push(b'!');
    push_int(out, msg.len() as i64);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(msg);
    out.extend_from_slice(b"\r\n");
}

/// Local copy of [`crate::reply_encode::push_int`] — keeps this file
/// independent of the RESP2 encoder module's private helpers (so they
/// can evolve separately) without taking a dep on a public re-export.
fn push_int(out: &mut Vec<u8>, n: i64) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let neg = n < 0;
    let mut v = n;
    while v != 0 {
        let digit = (v % 10).unsigned_abs() as u8;
        i -= 1;
        tmp[i] = b'0' + digit;
        v /= 10;
    }
    if neg {
        out.push(b'-');
    }
    out.extend_from_slice(&tmp[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_reply, Reply};

    /// Each encoder pairs with its parse_reply round-trip — byte-exact
    /// out, structurally-exact back in.
    #[test]
    fn map_header_round_trip() {
        let mut out = Vec::new();
        encode_map_header(&mut out, 2);
        // Build a full map: 2 pairs, each (Int, Bulk).
        crate::encode_integer(&mut out, 1);
        crate::encode_bulk(&mut out, b"a");
        crate::encode_integer(&mut out, 2);
        crate::encode_bulk(&mut out, b"b");
        assert_eq!(out, b"%2\r\n:1\r\n$1\r\na\r\n:2\r\n$1\r\nb\r\n");
        let (r, used) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(used, out.len());
        assert_eq!(
            r,
            Reply::Map(vec![
                (Reply::Int(1), Reply::Bulk(b"a".to_vec())),
                (Reply::Int(2), Reply::Bulk(b"b".to_vec())),
            ])
        );
    }

    #[test]
    fn set_header_round_trip() {
        let mut out = Vec::new();
        encode_set_header(&mut out, 3);
        for i in 1..=3 {
            crate::encode_integer(&mut out, i);
        }
        assert_eq!(out, b"~3\r\n:1\r\n:2\r\n:3\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(r, Reply::Set(vec![Reply::Int(1), Reply::Int(2), Reply::Int(3)]));
    }

    #[test]
    fn push_header_round_trip() {
        let mut out = Vec::new();
        encode_push_header(&mut out, 3);
        crate::encode_simple_string(&mut out, "message");
        crate::encode_bulk(&mut out, b"news");
        crate::encode_bulk(&mut out, b"hello");
        assert_eq!(out, b">3\r\n+message\r\n$4\r\nnews\r\n$5\r\nhello\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(
            r,
            Reply::Push(vec![
                Reply::Simple(b"message".to_vec()),
                Reply::Bulk(b"news".to_vec()),
                Reply::Bulk(b"hello".to_vec()),
            ])
        );
    }

    #[test]
    fn double_round_trip() {
        let mut out = Vec::new();
        // 1.5 avoids clippy::approx_constant flagging 3.14 as a PI proxy.
        encode_double(&mut out, 1.5);
        assert_eq!(out, b",1.5\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(r, Reply::Double(1.5));

        out.clear();
        encode_double(&mut out, 5.0); // integer-valued: no decimal point
        assert_eq!(out, b",5\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(r, Reply::Double(5.0));

        out.clear();
        encode_double(&mut out, f64::INFINITY);
        assert_eq!(out, b",inf\r\n");

        out.clear();
        encode_double(&mut out, f64::NEG_INFINITY);
        assert_eq!(out, b",-inf\r\n");

        out.clear();
        encode_double(&mut out, f64::NAN);
        assert_eq!(out, b",nan\r\n");
    }

    #[test]
    fn boolean_and_null_round_trip() {
        let mut out = Vec::new();
        encode_boolean(&mut out, true);
        assert_eq!(out, b"#t\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(r, Reply::Boolean(true));

        out.clear();
        encode_boolean(&mut out, false);
        assert_eq!(out, b"#f\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(r, Reply::Boolean(false));

        out.clear();
        encode_null(&mut out);
        assert_eq!(out, b"_\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(r, Reply::Null);
    }

    #[test]
    fn verbatim_round_trip() {
        let mut out = Vec::new();
        encode_verbatim(&mut out, *b"txt", b"Some string");
        assert_eq!(out, b"=15\r\ntxt:Some string\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(r, Reply::Verbatim { fmt: *b"txt", data: b"Some string".to_vec() });
    }

    #[test]
    fn big_number_round_trip() {
        let mut out = Vec::new();
        encode_big_number(&mut out, b"170141183460469231731687303715884105727");
        assert_eq!(out, b"(170141183460469231731687303715884105727\r\n");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(
            r,
            Reply::BigNumber(b"170141183460469231731687303715884105727".to_vec())
        );
    }

    #[test]
    fn blob_error_round_trip() {
        let mut out = Vec::new();
        encode_blob_error(&mut out, b"ERR bad thing\nwith newline");
        let (r, _) = parse_reply(&out).unwrap().unwrap();
        assert_eq!(r, Reply::BlobError(b"ERR bad thing\nwith newline".to_vec()));
    }
}
