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
///
/// ```
/// use kevy_verbs::reply::ERR_NOT_INT;
/// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
/// let mut store = kevy_store::Store::new();
/// let mut out = Vec::new();
/// kevy_verbs::exec(&mut store, b"INCRBY", &argv("INCRBY k many"), &mut out);
/// assert_eq!(out, format!("-{ERR_NOT_INT}\r\n").as_bytes());
/// ```
pub const ERR_NOT_INT: &str = "ERR value is not an integer or out of range";
/// Redis's reply to a token that should have been a float.
///
/// ```
/// use kevy_verbs::reply::ERR_NOT_FLOAT;
/// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
/// let mut store = kevy_store::Store::new();
/// let mut out = Vec::new();
/// kevy_verbs::exec(&mut store, b"INCRBYFLOAT", &argv("INCRBYFLOAT k many"), &mut out);
/// assert_eq!(out, format!("-{ERR_NOT_FLOAT}\r\n").as_bytes());
/// ```
pub const ERR_NOT_FLOAT: &str = "ERR value is not a valid float";
/// Redis's reply to a malformed option list.
///
/// ```
/// use kevy_verbs::reply::ERR_SYNTAX;
/// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
/// let mut store = kevy_store::Store::new();
/// let mut out = Vec::new();
/// kevy_verbs::exec(&mut store, b"SET", &argv("SET k v SOMETIMES"), &mut out);
/// assert_eq!(out, format!("-{ERR_SYNTAX}\r\n").as_bytes());
/// ```
pub const ERR_SYNTAX: &str = "ERR syntax error";
/// Redis's reply to a command against a key of another type.
///
/// ```
/// use kevy_verbs::reply::WRONGTYPE;
/// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
/// let mut store = kevy_store::Store::new();
/// let mut out = Vec::new();
/// kevy_verbs::exec(&mut store, b"SET", &argv("SET k abc"), &mut Vec::new());
/// kevy_verbs::exec(&mut store, b"LPUSH", &argv("LPUSH k x"), &mut out);
/// assert_eq!(out, format!("-{WRONGTYPE}\r\n").as_bytes());
/// ```
pub const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";
/// Redis's reply to a write refused under `maxmemory` with no eviction,
/// in the words client libraries match on.
///
/// ```
/// use kevy_resp::RespVersion;
/// use kevy_verbs::reply::{OOM_ERR, Scores, emit_zrange};
/// let mut out = Vec::new();
/// let refused = Err(kevy_store::StoreError::OutOfMemory);
/// emit_zrange(refused, Scores::Omitted, RespVersion::V2, &mut out);
/// assert_eq!(out, format!("-{OOM_ERR}\r\n").as_bytes());
/// ```
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

/// A keyspace error as its RESP error reply.
///
/// ```
/// let mut out = Vec::new();
/// kevy_verbs::reply::store_err(&mut out, kevy_store::StoreError::NoSuchKey);
/// assert_eq!(out, b"-ERR no such key\r\n");
/// ```
#[inline]
pub fn store_err(out: &mut Vec<u8>, e: StoreError) {
    encode_error(out, e.as_wire());
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

/// Whether a sorted-set range reply carries each member's score: the
/// presence or absence of `WITHSCORES`.
///
/// ```
/// use kevy_verbs::reply::Scores;
///
/// assert_eq!(Scores::default(), Scores::Omitted);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Scores {
    /// Members only (no `WITHSCORES`).
    ///
    /// ```
    /// use kevy_resp::RespVersion;
    /// use kevy_verbs::reply::{Scores, emit_zrange};
    /// let mut out = Vec::new();
    /// emit_zrange(Ok(vec![(b"m".to_vec(), 2.0)]), Scores::Omitted, RespVersion::V2, &mut out);
    /// assert_eq!(out, b"*1\r\n$1\r\nm\r\n");
    /// ```
    #[default]
    Omitted,
    /// Each member with its score (`WITHSCORES`).
    ///
    /// ```
    /// use kevy_resp::RespVersion;
    /// use kevy_verbs::reply::{Scores, emit_zrange};
    /// let mut out = Vec::new();
    /// emit_zrange(Ok(vec![(b"m".to_vec(), 2.0)]), Scores::Included, RespVersion::V3, &mut out);
    /// // RESP3 nests each pair, the score as a double
    /// assert_eq!(out, b"*1\r\n*2\r\n$1\r\nm\r\n,2\r\n");
    /// ```
    Included,
}

/// A `(member, score)` list as a range reply.
///
/// With [`Scores::Omitted`] both protocols get a flat array of members.
/// With [`Scores::Included`], RESP2 interleaves each score as a bulk
/// string, and RESP3 nests `[member, score]` pairs with the score as a
/// double.
///
/// ```
/// use kevy_resp::RespVersion;
/// use kevy_verbs::reply::{Scores, emit_zrange};
///
/// let mut out = Vec::new();
/// emit_zrange(Ok(vec![(b"m".to_vec(), 2.0)]), Scores::Included, RespVersion::V2, &mut out);
/// assert_eq!(out, b"*2\r\n$1\r\nm\r\n$1\r\n2\r\n");
/// ```
pub fn emit_zrange(
    res: Result<Vec<(Vec<u8>, f64)>, StoreError>,
    scores: Scores,
    proto: RespVersion,
    out: &mut Vec<u8>,
) {
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => match (scores, proto) {
            (Scores::Omitted, _) => {
                encode_array_len(out, items.len() as i64);
                for (m, _) in &items {
                    encode_bulk(out, m);
                }
            }
            (Scores::Included, RespVersion::V2) => {
                encode_array_len(out, (items.len() * 2) as i64);
                for (m, sc) in &items {
                    encode_bulk(out, m);
                    encode_bulk(out, &fmt_score(*sc));
                }
            }
            (Scores::Included, RespVersion::V3) => {
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
