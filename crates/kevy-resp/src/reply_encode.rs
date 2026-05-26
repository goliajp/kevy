//! Reply encoders: append RESP2-shaped bytes to a caller-owned `Vec<u8>`. The
//! caller (typically the reactor's per-cmd reply buffer) keeps amortised
//! capacity across commands, so these functions never allocate beyond the
//! initial reserve.

/// `+<s>\r\n`
pub fn encode_simple_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'+');
    out.extend_from_slice(s.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// `-<s>\r\n`
pub fn encode_error(out: &mut Vec<u8>, s: &str) {
    out.push(b'-');
    out.extend_from_slice(s.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// `:<n>\r\n`
pub fn encode_integer(out: &mut Vec<u8>, n: i64) {
    out.push(b':');
    push_int(out, n);
    out.extend_from_slice(b"\r\n");
}

/// `$<len>\r\n<data>\r\n`
pub fn encode_bulk(out: &mut Vec<u8>, data: &[u8]) {
    // Reserve the whole frame up front so a fresh reply buffer (the common case:
    // dispatch hands each command an empty `Vec`) fills without repeated reallocs
    // as it grows — the bulk reply is the hot GET path. 16 covers '$', the length
    // digits, and both CRLFs.
    out.reserve(data.len() + 16);
    out.push(b'$');
    push_int(out, data.len() as i64);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
}

/// `$-1\r\n` — the RESP2 null bulk string.
pub fn encode_null_bulk(out: &mut Vec<u8>) {
    out.extend_from_slice(b"$-1\r\n");
}

/// `*<len>\r\n` — an array header; follow with `len` encoded elements.
pub fn encode_array_len(out: &mut Vec<u8>, len: i64) {
    out.push(b'*');
    push_int(out, len);
    out.extend_from_slice(b"\r\n");
}

/// Encode a command as a RESP multi-bulk request (client → server):
/// `*N\r\n$len\r\n<arg>\r\n…`. The inverse of
/// [`parse_command`](crate::parse_command).
pub fn encode_command(out: &mut Vec<u8>, args: &[Vec<u8>]) {
    encode_array_len(out, args.len() as i64);
    for a in args {
        encode_bulk(out, a);
    }
}

/// Append the base-10 representation of `n` without allocating an intermediate
/// String. Handles `i64::MIN` correctly.
fn push_int(out: &mut Vec<u8>, n: i64) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    // Work in the negative domain so i64::MIN doesn't overflow.
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

    #[test]
    fn encoders_match_resp2() {
        let mut out = Vec::new();
        encode_simple_string(&mut out, "PONG");
        assert_eq!(out, b"+PONG\r\n");

        out.clear();
        encode_bulk(&mut out, b"hello");
        assert_eq!(out, b"$5\r\nhello\r\n");

        out.clear();
        encode_error(&mut out, "ERR nope");
        assert_eq!(out, b"-ERR nope\r\n");

        out.clear();
        encode_integer(&mut out, -1234);
        assert_eq!(out, b":-1234\r\n");

        out.clear();
        encode_integer(&mut out, i64::MIN);
        assert_eq!(out, b":-9223372036854775808\r\n");

        out.clear();
        encode_null_bulk(&mut out);
        assert_eq!(out, b"$-1\r\n");
    }
}
