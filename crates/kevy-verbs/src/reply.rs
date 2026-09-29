//! RESP replies in Redis's shapes and words.
//!
//! ```
//! let mut out = Vec::new();
//! kevy_verbs::reply::wrong_args(&mut out, "get");
//! assert_eq!(out, b"-ERR wrong number of arguments for 'get' command\r\n");
//! ```

use kevy_resp::{
    RespVersion, encode_array_len, encode_bulk, encode_double, encode_error, encode_integer,
};
use kevy_store::StoreError;

/// Redis's reply to a token that should have been an integer.
pub const ERR_NOT_INT: &str = "ERR value is not an integer or out of range";
/// Redis's reply to a token that should have been a float.
pub const ERR_NOT_FLOAT: &str = "ERR value is not a valid float";
/// Redis's reply to a malformed option list.
pub const ERR_SYNTAX: &str = "ERR syntax error";
/// Redis's reply to a command against a key of another type.
pub const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";
/// Redis's reply to a write refused under `maxmemory` with no eviction,
/// in the words client libraries match on.
pub const OOM_ERR: &str = "OOM command not allowed when used memory > 'maxmemory'.";

/// The arity refusal, naming the verb the way Redis does (lowercase).
///
/// ```
/// let mut out = Vec::new();
/// kevy_verbs::reply::wrong_args(&mut out, "get");
/// assert_eq!(out, b"-ERR wrong number of arguments for 'get' command\r\n");
/// ```
pub fn wrong_args(out: &mut Vec<u8>, cmd: &str) {
    encode_error(out, &format!("ERR wrong number of arguments for '{cmd}' command"));
}

/// The wire wording of a keyspace error.
///
/// ```
/// use kevy_store::StoreError;
/// assert!(kevy_verbs::reply::store_err_msg(&StoreError::WrongType).starts_with("WRONGTYPE"));
/// ```
pub fn store_err_msg(e: &StoreError) -> &'static str {
    e.as_wire()
}

/// A keyspace error as its RESP error reply.
///
/// ```
/// let mut out = Vec::new();
/// kevy_verbs::reply::store_err(&mut out, kevy_store::StoreError::NoSuchKey);
/// assert_eq!(out, b"-ERR no such key\r\n");
/// ```
#[inline]
pub fn store_err(out: &mut Vec<u8>, e: StoreError) {
    encode_error(out, store_err_msg(&e));
}

/// `:n` on success, the error reply otherwise.
///
/// ```
/// let mut out = Vec::new();
/// kevy_verbs::reply::emit_int_result(Ok(3), &mut out);
/// assert_eq!(out, b":3\r\n");
/// ```
#[inline]
pub fn emit_int_result(res: Result<i64, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(n) => encode_integer(out, n),
        Err(e) => store_err(out, e),
    }
}

/// An array of bulk strings on success, the error reply otherwise.
///
/// ```
/// let mut out = Vec::new();
/// kevy_verbs::reply::emit_bulk_array(Ok(vec![b"a".to_vec()]), &mut out);
/// assert_eq!(out, b"*1\r\n$1\r\na\r\n");
/// ```
pub fn emit_bulk_array(res: Result<Vec<Vec<u8>>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(items) => {
            encode_array_len(out, items.len() as i64);
            for it in &items {
                encode_bulk(out, it);
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// A `(member, score)` list as a range reply.
///
/// Without `withscores` both protocols get a flat array of members.
/// With it, RESP2 interleaves each score as a bulk string, and RESP3
/// nests `[member, score]` pairs with the score as a double.
///
/// ```
/// use kevy_resp::RespVersion;
/// let mut out = Vec::new();
/// kevy_verbs::reply::emit_zrange(Ok(vec![(b"m".to_vec(), 2.0)]), true, RespVersion::V2, &mut out);
/// assert_eq!(out, b"*2\r\n$1\r\nm\r\n$1\r\n2\r\n");
/// ```
pub fn emit_zrange(
    res: Result<Vec<(Vec<u8>, f64)>, StoreError>,
    withscores: bool,
    proto: RespVersion,
    out: &mut Vec<u8>,
) {
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => match (withscores, proto) {
            (false, _) => {
                encode_array_len(out, items.len() as i64);
                for (m, _) in &items {
                    encode_bulk(out, m);
                }
            }
            (true, RespVersion::V2) => {
                encode_array_len(out, (items.len() * 2) as i64);
                for (m, sc) in &items {
                    encode_bulk(out, m);
                    encode_bulk(out, &fmt_score(*sc));
                }
            }
            (true, RespVersion::V3) => {
                encode_array_len(out, items.len() as i64);
                for (m, sc) in &items {
                    encode_array_len(out, 2);
                    encode_bulk(out, m);
                    encode_double(out, *sc);
                }
            }
        },
    }
}

/// The `[cursor, [elements…]]` envelope of a one-batch `HSCAN` /
/// `SSCAN` / `ZSCAN`: the cursor is always "0".
pub(crate) fn scan_page(out: &mut Vec<u8>, elems: &[Vec<u8>]) {
    encode_array_len(out, 2);
    encode_bulk(out, b"0");
    encode_array_len(out, elems.len() as i64);
    for e in elems {
        encode_bulk(out, e);
    }
}

/// A score as Redis prints it: integral values carry no decimal point.
///
/// ```
/// use kevy_verbs::reply::fmt_score;
/// assert_eq!(fmt_score(3.0), b"3");
/// assert_eq!(fmt_score(2.5), b"2.5");
/// assert_eq!(fmt_score(f64::INFINITY), b"inf");
/// ```
pub fn fmt_score(s: f64) -> Vec<u8> {
    if s.is_infinite() {
        return if s > 0.0 { b"inf".to_vec() } else { b"-inf".to_vec() };
    }
    // exact comparison on purpose: an epsilon would change the wire shape
    #[allow(clippy::float_cmp)]
    let is_integer_valued = s == s.trunc();
    if is_integer_valued && s.abs() < 1e17 {
        return (s as i64).to_string().into_bytes();
    }
    format!("{s}").into_bytes()
}
