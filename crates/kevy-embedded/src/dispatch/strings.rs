//! String-family verbs: SET variants, counters, ranges, multi-key.

use std::time::Duration;

use crate::store::Store;
use crate::KevyResult;

use super::util::{
    arg_f64, arg_i64, arr, bulk, emit_int, err, fmt_score, kevy_err, nil, opt_bulk, rest, simple,
    wrong_args, ERR_NOT_FLOAT, ERR_NOT_INT, ERR_SYNTAX,
};

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
                    Ok(v) => bulk(out, &fmt_score(v)),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                err(out, ERR_NOT_FLOAT);
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
                    Ok(v) => bulk(out, &v),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                err(out, ERR_NOT_INT);
            }
        }
        b"SETRANGE" => cmd_setrange(s, argv, out),
        b"MGET" => {
            if argv.len() < 2 {
                wrong_args(out, "mget");
            } else {
                match s.mget(&rest(argv, 1)) {
                    Ok(vals) => {
                        arr(out, vals.len());
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
                    Ok(()) => simple(out, "OK"),
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
                    return err(out, ERR_SYNTAX);
                };
                let Some(n) = arg_i64(raw).filter(|&n| n > 0) else {
                    return err(out, "ERR invalid expire time in 'set' command");
                };
                let ms = if opt == b"EX" { n.saturating_mul(1000) } else { n };
                expire = Some(Duration::from_millis(ms as u64));
                i += 1;
            }
            _ => return err(out, ERR_SYNTAX),
        }
        i += 1;
    }
    if nx && xx {
        return err(out, ERR_SYNTAX);
    }
    match set_composed(s, &argv[1], &argv[2], expire, nx, xx) {
        Ok(true) => simple(out, "OK"),
        Ok(false) => nil(out), // NX/XX condition not met
        Err(e) => kevy_err(out, &e),
    }
}

fn set_composed(
    s: &Store,
    key: &[u8],
    val: &[u8],
    expire: Option<Duration>,
    nx: bool,
    xx: bool,
) -> KevyResult<bool> {
    if nx {
        let ok = s.setnx(key, val)?;
        if ok && let Some(d) = expire {
            s.expire(key, d)?;
        }
        return Ok(ok);
    }
    if xx && s.exists(&[key])? == 0 {
        return Ok(false);
    }
    match expire {
        Some(d) => s.set_with_ttl(key, val, d),
        None => s.set(key, val),
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
        return err(out, ERR_NOT_INT);
    };
    if negate {
        let Some(neg) = delta.checked_neg() else {
            return err(out, "ERR decrement would overflow");
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
                return err(out, ERR_SYNTAX);
            }
            let Some(n) = arg_i64(&argv[3]).filter(|&n| n > 0) else {
                return err(out, "ERR invalid expire time in 'getex' command");
            };
            let ms = if opt == b"EX" { n.saturating_mul(1000) } else { n };
            match s.getex(&argv[1], Duration::from_millis(ms as u64)) {
                Ok(v) => opt_bulk(out, v),
                Err(e) => kevy_err(out, &e),
            }
        }
        0 | 1 => wrong_args(out, "getex"),
        _ => err(out, ERR_SYNTAX),
    }
}

fn cmd_setrange(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() != 4 {
        return wrong_args(out, "setrange");
    }
    let Some(off) = arg_i64(&argv[2]) else {
        return err(out, ERR_NOT_INT);
    };
    if off < 0 {
        return err(out, "ERR offset is out of range");
    }
    emit_int(out, s.setrange(&argv[1], off as u64, &argv[3]).map(|n| n as i64));
}
