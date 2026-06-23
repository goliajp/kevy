//! Minimal RESP2 encoder for kevy-lua. Hand-rolled because the
//! reply set we need is a tiny subset of `kevy-resp` and keeping it
//! self-contained avoids a circular dependency hazard during P1-P4
//! development.
//!
//! P5+ may switch to the full `kevy-resp` encoder once the bridge
//! command surface settles. The encoded bytes are wire-equivalent
//! either way.
//!
//! This module is **pure** — encoders only, no luna types. The
//! Lua-aware marshaling (table → array, `{ok=}` / `{err=}`
//! recognition) lives in the parent module's `marshal` function
//! which composes these encoders.

/// `:N\r\n` — RESP integer.
pub(crate) fn integer(n: i64) -> Vec<u8> {
    format!(":{n}\r\n").into_bytes()
}

/// `$-1\r\n` — RESP nil bulk.
pub(crate) fn nil_bulk() -> Vec<u8> {
    b"$-1\r\n".to_vec()
}

/// `$N\r\nBYTES\r\n` — RESP bulk string. Binary-safe.
pub(crate) fn bulk(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 16);
    out.push(b'$');
    out.extend_from_slice(bytes.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
    out
}

/// `+MSG\r\n` — RESP simple string. MSG must NOT contain CR or LF
/// (RESP2 simple-string grammar). Caller is responsible for that —
/// when in doubt, use [`bulk`] which is binary-safe.
pub(crate) fn simple_string(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + 3);
    out.push(b'+');
    out.extend_from_slice(msg);
    out.extend_from_slice(b"\r\n");
    out
}

/// `*N\r\n` — RESP array header. The N elements follow, each
/// individually encoded. N = -1 produces the null-array form
/// `*-1\r\n` (kevy never emits this from EVAL, but it's part of the
/// RESP2 grammar).
pub(crate) fn array_header(n: i64) -> Vec<u8> {
    format!("*{n}\r\n").into_bytes()
}

/// `-ERR <msg>\r\n` — RESP simple error. Caller should not include
/// the `ERR ` prefix unless they want a custom kind (e.g. `WRONGTYPE`,
/// `NOSCRIPT`).
pub(crate) fn err(msg: &[u8]) -> Vec<u8> {
    // Convention: if the msg already starts with an upper-case kind
    // token (NOSCRIPT, WRONGTYPE, READONLY, …) we pass it through;
    // otherwise we prepend `ERR `.
    let mut out = Vec::with_capacity(msg.len() + 8);
    out.push(b'-');
    if needs_err_prefix(msg) {
        out.extend_from_slice(b"ERR ");
    }
    out.extend_from_slice(msg);
    out.extend_from_slice(b"\r\n");
    out
}

fn needs_err_prefix(msg: &[u8]) -> bool {
    // First whitespace-delimited token starts with a sequence of
    // upper-case ASCII letters → caller wrote their own kind.
    let token: &[u8] = msg.split(|b| *b == b' ').next().unwrap_or(b"");
    if token.is_empty() {
        return true;
    }
    !token.iter().all(|b| b.is_ascii_uppercase())
}

/// Encode a finite f64 as a RESP integer when it round-trips through
/// i64 losslessly, otherwise as a bulk string. Lua 5.1 has no integer
/// subtype, so every `return 1` arrives as `Value::Float(1.0)` — the
/// caller has to pick the on-wire shape, which this helper does
/// per the Redis-EVAL convention.
pub(crate) fn float(f: f64) -> Vec<u8> {
    if f.is_finite() && f.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&f) {
        integer(f as i64)
    } else {
        bulk(format!("{f}").as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_prefix_added_for_lowercase_msg() {
        assert_eq!(err(b"oops something broke"), b"-ERR oops something broke\r\n");
    }

    #[test]
    fn err_passthrough_for_kind_prefix() {
        assert_eq!(err(b"NOSCRIPT no script"), b"-NOSCRIPT no script\r\n");
        assert_eq!(err(b"WRONGTYPE bad"), b"-WRONGTYPE bad\r\n");
    }

    #[test]
    fn bulk_binary_safe() {
        let r = bulk(&[0, 1, 2, 0xff]);
        assert_eq!(r, b"$4\r\n\x00\x01\x02\xff\r\n");
    }

    #[test]
    fn bulk_empty_string() {
        assert_eq!(bulk(b""), b"$0\r\n\r\n");
    }

    #[test]
    fn integer_max_min() {
        assert_eq!(integer(i64::MAX), format!(":{}\r\n", i64::MAX).into_bytes());
        assert_eq!(integer(i64::MIN), format!(":{}\r\n", i64::MIN).into_bytes());
    }

    #[test]
    fn simple_string_basic() {
        assert_eq!(simple_string(b"OK"), b"+OK\r\n");
        assert_eq!(simple_string(b""), b"+\r\n");
    }

    #[test]
    fn array_header_shapes() {
        assert_eq!(array_header(0), b"*0\r\n");
        assert_eq!(array_header(3), b"*3\r\n");
        assert_eq!(array_header(-1), b"*-1\r\n");
    }

    #[test]
    fn float_round_trips_to_integer_for_integral() {
        assert_eq!(float(1.0), b":1\r\n");
        assert_eq!(float(0.0), b":0\r\n");
        assert_eq!(float(-42.0), b":-42\r\n");
    }

    #[test]
    fn float_falls_back_to_bulk_for_non_integral() {
        assert_eq!(float(1.5), b"$3\r\n1.5\r\n");
        assert_eq!(float(f64::NAN).starts_with(b"$"), true);
        assert_eq!(float(f64::INFINITY).starts_with(b"$"), true);
    }
}
