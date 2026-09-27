//! String-family verbs: SET variants, counters, ranges, multi-key.

use std::time::Duration;

use crate::store::Store;

use super::{emit_int, kevy_err, opt_bulk, rest};
use kevy_resp::{
    encode_array_len, encode_bulk, encode_error, encode_null_bulk, encode_simple_string,
};
use kevy_verbs::args::{arg_f64, arg_i64};
use kevy_verbs::reply::{ERR_NOT_FLOAT, ERR_NOT_INT, ERR_SYNTAX, fmt_score, wrong_args};

/// One string-family request; `false` = verb not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per string verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"SET" => cmd_set(s, argv, out),
        b"GET" => match argv.len() {
            2 => match s.get(&argv[1]) {
                Ok(v) => opt_bulk(out, v),
                Err(e) => kevy_err(out, &e),
            },
            _ => wrong_args(out, "get"),
        },
        b"SETNX" => {
            if argv.len() == 3 {
                match s.setnx(&argv[1], &argv[2]) {
                    Ok(set) => emit_int(out, Ok(i64::from(set))),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "setnx");
            }
        }
        b"APPEND" => {
            if argv.len() == 3 {
                emit_int(out, s.append(&argv[1], &argv[2]).map(|n| n as i64));
            } else {
                wrong_args(out, "append");
            }
        }
        b"STRLEN" => {
            if argv.len() == 2 {
                emit_int(out, s.strlen(&argv[1]).map(|n| n as i64));
            } else {
                wrong_args(out, "strlen");
            }
        }
        b"INCR" => cmd_incr(s, argv, 1, "incr", out),
        b"DECR" => cmd_incr(s, argv, -1, "decr", out),
        b"INCRBY" => cmd_incr_by(s, argv, false, "incrby", out),
        b"DECRBY" => cmd_incr_by(s, argv, true, "decrby", out),
        b"INCRBYFLOAT" => {
            if argv.len() != 3 {
                wrong_args(out, "incrbyfloat");
            } else if let Some(d) = arg_f64(&argv[2]) {
                match s.incrbyfloat(&argv[1], d) {
                    Ok(v) => encode_bulk(out, &fmt_score(v)),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                encode_error(out, ERR_NOT_FLOAT);
            }
        }
        b"GETSET" => {
            if argv.len() == 3 {
                match s.getset(&argv[1], &argv[2]) {
                    Ok(v) => opt_bulk(out, v),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "getset");
            }
        }
        b"GETDEL" => {
            if argv.len() == 2 {
                match s.getdel(&argv[1]) {
                    Ok(v) => opt_bulk(out, v),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "getdel");
            }
        }
        b"GETEX" => cmd_getex(s, argv, out),
        b"GETRANGE" => {
            if argv.len() != 4 {
                wrong_args(out, "getrange");
            } else if let (Some(a), Some(b)) = (arg_i64(&argv[2]), arg_i64(&argv[3])) {
                match s.getrange(&argv[1], a, b) {
                    Ok(v) => encode_bulk(out, &v),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"SETRANGE" => cmd_setrange(s, argv, out),
        b"MGET" => {
            if argv.len() < 2 {
                wrong_args(out, "mget");
            } else {
                match s.mget(&rest(argv, 1)) {
                    Ok(vals) => {
                        encode_array_len(out, vals.len() as i64);
                        for v in vals {
                            opt_bulk(out, v);
                        }
                    }
                    Err(e) => kevy_err(out, &e),
                }
            }
        }
        b"MSET" => {
            if argv.len() < 3 || argv.len().is_multiple_of(2) {
                wrong_args(out, "mset");
            } else {
                let pairs: Vec<(&[u8], &[u8])> = (1..argv.len())
                    .step_by(2)
                    .map(|i| (argv[i].as_slice(), argv[i + 1].as_slice()))
                    .collect();
                match s.mset(&pairs) {
                    Ok(()) => encode_simple_string(out, "OK"),
                    Err(e) => kevy_err(out, &e),
                }
            }
        }
        _ => return false,
    }
    true
}

/// `SET key value [EX s | PX ms] [NX | XX]` — the server's option
/// grammar, composed over the typed facades (`set` / `set_with_ttl` /
/// `setnx`; the XX form checks existence first).
fn cmd_set(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 3 {
        return wrong_args(out, "set");
    }
    let mut expire: Option<Duration> = None;
    let (mut nx, mut xx) = (false, false);
    let mut i = 3;
    while i < argv.len() {
        match argv[i].to_ascii_uppercase().as_slice() {
            b"NX" => nx = true,
            b"XX" => xx = true,
            opt @ (b"EX" | b"PX") => {
                let Some(raw) = argv.get(i + 1) else {
                    return encode_error(out, ERR_SYNTAX);
                };
                let Some(n) = arg_i64(raw).filter(|&n| n > 0) else {
                    return encode_error(out, "ERR invalid expire time in 'set' command");
                };
                let ms = if opt == b"EX" { n.saturating_mul(1000) } else { n };
                expire = Some(Duration::from_millis(ms as u64));
                i += 1;
            }
            _ => return encode_error(out, ERR_SYNTAX),
        }
        i += 1;
    }
    if nx && xx {
        return encode_error(out, ERR_SYNTAX);
    }
    match s.set_opts(&argv[1], &argv[2], expire, nx, xx) {
        Ok(true) => encode_simple_string(out, "OK"),
        Ok(false) => encode_null_bulk(out), // NX/XX condition not met
        Err(e) => kevy_err(out, &e),
    }
}

fn cmd_incr(s: &Store, argv: &[Vec<u8>], delta: i64, name: &str, out: &mut Vec<u8>) {
    if argv.len() != 2 {
        return wrong_args(out, name);
    }
    emit_int(out, s.incr_by(&argv[1], delta));
}

fn cmd_incr_by(s: &Store, argv: &[Vec<u8>], negate: bool, name: &str, out: &mut Vec<u8>) {
    if argv.len() != 3 {
        return wrong_args(out, name);
    }
    let Some(mut delta) = arg_i64(&argv[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    if negate {
        let Some(neg) = delta.checked_neg() else {
            return encode_error(out, "ERR decrement would overflow");
        };
        delta = neg;
    }
    emit_int(out, s.incr_by(&argv[1], delta));
}

/// `GETEX key [EX s | PX ms]` — bare form is a plain read; the typed
/// facade only carries relative TTLs (EXAT/PXAT/PERSIST are not
/// exposed embedded).
fn cmd_getex(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    match argv.len() {
        2 => match s.get(&argv[1]) {
            Ok(v) => opt_bulk(out, v),
            Err(e) => kevy_err(out, &e),
        },
        4 => {
            let opt = argv[2].to_ascii_uppercase();
            if opt != b"EX" && opt != b"PX" {
                return encode_error(out, ERR_SYNTAX);
            }
            let Some(n) = arg_i64(&argv[3]).filter(|&n| n > 0) else {
                return encode_error(out, "ERR invalid expire time in 'getex' command");
            };
            let ms = if opt == b"EX" { n.saturating_mul(1000) } else { n };
            match s.getex(&argv[1], Duration::from_millis(ms as u64)) {
                Ok(v) => opt_bulk(out, v),
                Err(e) => kevy_err(out, &e),
            }
        }
        0 | 1 => wrong_args(out, "getex"),
        _ => encode_error(out, ERR_SYNTAX),
    }
}

fn cmd_setrange(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() != 4 {
        return wrong_args(out, "setrange");
    }
    let Some(off) = arg_i64(&argv[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    if off < 0 {
        return encode_error(out, "ERR offset is out of range");
    }
    emit_int(out, s.setrange(&argv[1], off as u64, &argv[3]).map(|n| n as i64));
}
