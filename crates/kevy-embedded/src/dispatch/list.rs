//! List-family verbs.

use crate::store::Store;

use super::{emit_bulk_array, emit_int, kevy_err, opt_bulk, rest};
use kevy_resp::{
    encode_array_len, encode_bulk, encode_error, encode_null_bulk, encode_simple_string,
};
use kevy_verbs::args::arg_i64;
use kevy_verbs::reply::{ERR_NOT_INT, ERR_SYNTAX, wrong_args};

/// One list-family request; `false` = verb not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per list verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"LPUSH" => {
            if argv.len() < 3 {
                wrong_args(out, "lpush");
            } else {
                emit_int(out, s.lpush(&argv[1], &rest(argv, 2)).map(|n| n as i64));
            }
        }
        b"RPUSH" => {
            if argv.len() < 3 {
                wrong_args(out, "rpush");
            } else {
                emit_int(out, s.rpush(&argv[1], &rest(argv, 2)).map(|n| n as i64));
            }
        }
        b"LPOP" => cmd_pop(s, argv, false, out),
        b"RPOP" => cmd_pop(s, argv, true, out),
        b"LLEN" => {
            if argv.len() == 2 {
                emit_int(out, s.llen(&argv[1]).map(|n| n as i64));
            } else {
                wrong_args(out, "llen");
            }
        }
        b"LINDEX" => {
            if argv.len() != 3 {
                wrong_args(out, "lindex");
            } else if let Some(i) = arg_i64(&argv[2]) {
                match s.lindex(&argv[1], i) {
                    Ok(v) => opt_bulk(out, v),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LRANGE" => {
            if argv.len() != 4 {
                wrong_args(out, "lrange");
            } else if let (Some(a), Some(b)) = (arg_i64(&argv[2]), arg_i64(&argv[3])) {
                emit_bulk_array(out, s.lrange(&argv[1], a, b));
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LSET" => {
            if argv.len() != 4 {
                wrong_args(out, "lset");
            } else if let Some(i) = arg_i64(&argv[2]) {
                match s.lset(&argv[1], i, &argv[3]) {
                    Ok(()) => encode_simple_string(out, "OK"),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LREM" => {
            if argv.len() != 4 {
                wrong_args(out, "lrem");
            } else if let Some(c) = arg_i64(&argv[2]) {
                emit_int(out, s.lrem(&argv[1], c, &argv[3]).map(|n| n as i64));
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LTRIM" => {
            if argv.len() != 4 {
                wrong_args(out, "ltrim");
            } else if let (Some(a), Some(b)) = (arg_i64(&argv[2]), arg_i64(&argv[3])) {
                match s.ltrim(&argv[1], a, b) {
                    Ok(()) => encode_simple_string(out, "OK"),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LINSERT" => cmd_linsert(s, argv, out),
        _ => return false,
    }
    true
}

/// `LPOP`/`RPOP key [count]` — single bulk without count, array (or
/// `*-1` on empty) with it. Mirrors the server's `cmd_pop`.
fn cmd_pop(s: &Store, argv: &[Vec<u8>], tail: bool, out: &mut Vec<u8>) {
    let name = if tail { "rpop" } else { "lpop" };
    if argv.len() < 2 || argv.len() > 3 {
        return wrong_args(out, name);
    }
    let count_given = argv.len() == 3;
    let count = if count_given {
        match arg_i64(&argv[2]) {
            Some(c) if c >= 0 => c as usize,
            _ => return encode_error(out, "ERR value is out of range, must be positive"),
        }
    } else {
        1
    };
    let res = if tail { s.rpop(&argv[1], count) } else { s.lpop(&argv[1], count) };
    match res {
        Err(e) => kevy_err(out, &e),
        Ok(items) => {
            if count_given {
                if items.is_empty() {
                    encode_array_len(out, -1); // key absent / empty
                } else {
                    emit_bulk_array(out, Ok(items));
                }
            } else {
                match items.into_iter().next() {
                    Some(v) => encode_bulk(out, &v),
                    None => encode_null_bulk(out),
                }
            }
        }
    }
}

/// `LINSERT key BEFORE|AFTER pivot value`.
fn cmd_linsert(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() != 5 {
        return wrong_args(out, "linsert");
    }
    let before = if argv[2].eq_ignore_ascii_case(b"BEFORE") {
        true
    } else if argv[2].eq_ignore_ascii_case(b"AFTER") {
        false
    } else {
        return encode_error(out, ERR_SYNTAX);
    };
    emit_int(out, s.linsert(&argv[1], before, &argv[3], &argv[4]));
}
