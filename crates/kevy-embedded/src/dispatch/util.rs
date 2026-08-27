//! Shared RESP encoding + argv parsing helpers for the embedded
//! full-surface dispatcher. Wording and wire shapes mirror the server
//! (`kevy` crate) byte for byte — the oracle test in
//! `tests/dispatch_oracle.rs` holds the two surfaces together.

use crate::{KevyError, KevyResult, StoreError};

use kevy_store::ScoreBound;

pub(super) const ERR_NOT_INT: &str = "ERR value is not an integer or out of range";
pub(super) const WRONGTYPE: &str =
    "WRONGTYPE Operation against a key holding the wrong kind of value";
pub(super) const ERR_NOT_FLOAT: &str = "ERR value is not a valid float";
pub(super) const ERR_SYNTAX: &str = "ERR syntax error";
pub(super) const OOM_ERR: &str = "OOM command not allowed when used memory > 'maxmemory'.";

// ---- RESP encoders -----------------------------------------------------

pub(super) fn bulk(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", b.len()).as_bytes());
    out.extend_from_slice(b);
    out.extend_from_slice(b"\r\n");
}

pub(super) fn opt_bulk(out: &mut Vec<u8>, v: Option<Vec<u8>>) {
    match v {
        Some(b) => bulk(out, &b),
        None => nil(out),
    }
}

pub(super) fn int(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(format!(":{v}\r\n").as_bytes());
}

pub(super) fn arr(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(format!("*{n}\r\n").as_bytes());
}

pub(super) fn simple(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(format!("+{s}\r\n").as_bytes());
}

pub(super) fn nil(out: &mut Vec<u8>) {
    out.extend_from_slice(b"$-1\r\n");
}

pub(super) fn nil_array(out: &mut Vec<u8>) {
    out.extend_from_slice(b"*-1\r\n");
}

pub(super) fn err(out: &mut Vec<u8>, msg: &str) {
    out.extend_from_slice(format!("-{msg}\r\n").as_bytes());
}

/// The verb as Redis names it in an error: argv[0], lowercased.
pub(super) fn verb_name(argv: &[Vec<u8>]) -> String {
    String::from_utf8_lossy(argv.first().map(Vec::as_slice).unwrap_or(b"")).to_lowercase()
}

pub(super) fn wrong_args(out: &mut Vec<u8>, cmd: &str) {
    err(out, &format!("ERR wrong number of arguments for '{cmd}' command"));
}

// ---- error mapping -----------------------------------------------------

/// Encode a `KevyError` with the server's RESP wording (`cmd::store_err`
/// for the `Store` variants; the replica guard keeps its bare
/// `READONLY …` prefix, Redis-style).
pub(super) fn kevy_err(out: &mut Vec<u8>, e: &KevyError) {
    let msg: String = match e {
        KevyError::Store(se) => {
            return err(out, store_err_msg(se));
        }
        KevyError::ReadOnly => {
            return err(out, "READONLY You can't write against a read only replica");
        }
        KevyError::InvalidInput(m) | KevyError::NotFound(m) | KevyError::Unsupported(m) => {
            format!("ERR {m}")
        }
        KevyError::Io(ioe) => {
            // Catalog errors ride io::Error with an already-prefixed
            // "ERR …" message — pass those through unwrapped.
            let m = ioe.to_string();
            if m.starts_with("ERR ") { m } else { format!("ERR {m}") }
        }
        other => format!("ERR {other}"),
    };
    err(out, &msg);
}

pub(super) fn store_err_msg(e: &StoreError) -> &'static str {
    match e {
        StoreError::WrongType => WRONGTYPE,
        StoreError::NotInteger => ERR_NOT_INT,
        StoreError::Overflow => "ERR increment or decrement would overflow",
        StoreError::OutOfRange => "ERR index out of range",
        StoreError::NoSuchKey => "ERR no such key",
        StoreError::NotFloat => ERR_NOT_FLOAT,
        StoreError::OutOfMemory => OOM_ERR,
    }
}

// ---- result emitters ---------------------------------------------------

pub(super) fn emit_int(out: &mut Vec<u8>, res: KevyResult<i64>) {
    match res {
        Ok(n) => int(out, n),
        Err(e) => kevy_err(out, &e),
    }
}

pub(super) fn emit_bulk_array(out: &mut Vec<u8>, res: KevyResult<Vec<Vec<u8>>>) {
    match res {
        Ok(items) => {
            arr(out, items.len());
            for it in &items {
                bulk(out, it);
            }
        }
        Err(e) => kevy_err(out, &e),
    }
}

/// `(member, score)` list in the server's RESP2 `ZRANGE` shape: flat
/// bulks, scores interleaved only under `WITHSCORES`.
pub(super) fn emit_scored(out: &mut Vec<u8>, items: &[(Vec<u8>, f64)], withscores: bool) {
    arr(out, items.len() * if withscores { 2 } else { 1 });
    for (m, sc) in items {
        bulk(out, m);
        if withscores {
            bulk(out, &fmt_score(*sc));
        }
    }
}

// ---- argv parsers ------------------------------------------------------

pub(super) fn arg_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

pub(super) fn arg_u64(b: &[u8]) -> Option<u64> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

/// f64 argument, `inf` forms accepted, NaN rejected (server `arg_f64`).
pub(super) fn arg_f64(b: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(b).ok()?.trim();
    let f: f64 = match s.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
        "-inf" | "-infinity" => f64::NEG_INFINITY,
        _ => s.parse().ok()?,
    };
    if f.is_nan() { None } else { Some(f) }
}

/// `ZRANGEBYSCORE`/`ZCOUNT` bound: a leading `(` means exclusive.
pub(super) fn parse_score_bound(b: &[u8]) -> Option<ScoreBound> {
    match b.strip_prefix(b"(") {
        Some(rest) => Some(ScoreBound { value: arg_f64(rest)?, exclusive: true }),
        None => Some(ScoreBound { value: arg_f64(b)?, exclusive: false }),
    }
}

/// Score formatting matching the server's `fmt_score` (and the store's
/// `fmt_num`): integral values carry no decimal point.
pub(super) fn fmt_score(s: f64) -> Vec<u8> {
    if s.is_infinite() {
        return if s > 0.0 { b"inf".to_vec() } else { b"-inf".to_vec() };
    }
    #[allow(clippy::float_cmp)] // bit-exact contract, same as the server
    let is_integer_valued = s == s.trunc();
    if is_integer_valued && s.abs() < 1e17 {
        return (s as i64).to_string().into_bytes();
    }
    format!("{s}").into_bytes()
}

/// `argv[from..]` as borrowed slices (`cmd::rest_borrowed`).
pub(super) fn rest(argv: &[Vec<u8>], from: usize) -> Vec<&[u8]> {
    argv[from..].iter().map(Vec::as_slice).collect()
}
