//! Minimal RESP2 encoder for kevy-lua. Hand-rolled because the
//! reply set we need is a tiny subset of `kevy-resp` and keeping it
//! self-contained avoids a circular dependency hazard during P1-P4
//! development.
//!
//! P5+ may switch to the full `kevy-resp` encoder once the bridge
//! command surface settles. The encoded bytes are wire-equivalent
//! either way.

use luna_core::runtime::value::Value;

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

/// Marshal a luna `Value` into a RESP reply per the kevy-lua P1
/// rules. Mirrors the Redis EVAL marshalling table from the RFC §
/// "Marshaling — RESP ↔ Lua":
///
/// | Lua             | RESP                  |
/// |-----------------|-----------------------|
/// | nil             | `$-1\r\n` (nil bulk)  |
/// | boolean true    | `:1\r\n`              |
/// | boolean false   | `$-1\r\n` (nil bulk)  |
/// | integer         | `:N\r\n`              |
/// | float           | bulk string           |
/// | string          | bulk string           |
/// | `{ok=...}`      | simple string         |
/// | `{err=...}`     | error                 |
/// | array table     | (P2 — first-nil rule) |
///
/// `{ok=...}` / `{err=...}` table detection and array marshalling
/// land in P2; for now any `Table` value falls back to `nil bulk` so
/// the encoder always produces well-formed RESP.
pub(crate) fn reply_from_value(v: Value) -> Vec<u8> {
    match v {
        Value::Nil => nil_bulk(),
        Value::Bool(true) => integer(1),
        Value::Bool(false) => nil_bulk(),
        Value::Int(n) => integer(n),
        Value::Float(f) => {
            // Lua 5.1 returns every numeric literal as a Float. When
            // the value round-trips through i64 losslessly, present
            // it as a RESP integer (matches Redis behaviour). This
            // is what makes `EVAL "return 1" 0` produce `:1\r\n` even
            // though luna 5.1 hands us `Value::Float(1.0)`.
            if f.is_finite() && f.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&f)
            {
                integer(f as i64)
            } else {
                bulk(format!("{f}").as_bytes())
            }
        }
        Value::Str(s) => bulk(s.as_bytes()),
        Value::Table(_) | Value::Closure(_) | Value::Native(_) | Value::Coro(_)
        | Value::Userdata(_) | Value::LightUserdata(_) => nil_bulk(),
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
}
