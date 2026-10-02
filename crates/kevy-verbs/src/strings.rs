//! String commands, including the byte-range access Redis gives them
//! (`GETRANGE` / `SETRANGE`).

use std::time::Duration;

use kevy_resp::{
    ArgvView, encode_bulk, encode_error, encode_integer, encode_null_bulk, encode_simple_string,
};
use kevy_store::Store;

use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, emit_int_result, store_err, wrong_args};
use crate::{Effect, changed};

/// One string command; `None` = the verb is not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per string verb.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        b"GET" => {
            get(store, args, out);
            Effect::Read
        }
        b"SET" => crate::set_options::set(store, args, out),
        b"DIGEST" => crate::digest::exec(store, args, out),
        b"APPEND" => {
            if args.len() == 3 {
                emit_int_result(store.append(&args[1], &args[2]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "append");
            }
            Effect::Write
        }
        b"STRLEN" => {
            if args.len() == 2 {
                emit_int_result(store.strlen(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "strlen");
            }
            Effect::Read
        }
        b"INCR" => incr(store, args, 1, "incr", out),
        b"DECR" => incr(store, args, -1, "decr", out),
        b"INCRBY" => incr_by(store, args, false, "incrby", out),
        b"DECRBY" => incr_by(store, args, true, "decrby", out),
        b"SETNX" => {
            if args.len() != 3 {
                wrong_args(out, "setnx");
                return Some(Effect::Unchanged);
            }
            let set = store.set_slice(&args[1], &args[2], None, kevy_store::SetCondition::IfAbsent);
            encode_integer(out, i64::from(set));
            changed(set)
        }
        b"SETEX" => setex(store, args, 1000, "setex", out),
        b"PSETEX" => setex(store, args, 1, "psetex", out),
        b"GETSET" => {
            if args.len() == 3 {
                match store.getset(&args[1], args[2].to_vec()) {
                    Ok(Some(v)) => encode_bulk(out, &v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                wrong_args(out, "getset");
            }
            Effect::Write
        }
        b"GETDEL" => getdel(store, args, out),
        b"INCRBYFLOAT" => {
            if args.len() != 3 {
                wrong_args(out, "incrbyfloat");
                return Some(Effect::Unchanged);
            }
            match store.incr_by_float_text(&args[1], &args[2]) {
                Ok(v) => encode_bulk(out, &v),
                Err(e) => {
                    store_err(out, e);
                    return Some(Effect::Unchanged);
                }
            }
            Effect::Write
        }
        b"GETRANGE" | b"SUBSTR" => {
            if args.len() != 4 {
                wrong_args(out, if cmd == b"SUBSTR" { "substr" } else { "getrange" });
            } else if let (Some(a), Some(b)) = (arg_i64(&args[2]), arg_i64(&args[3])) {
                match store.getrange_borrowed(&args[1], a, b) {
                    Ok(v) => encode_bulk(out, &v),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
            Effect::Read
        }
        b"SETRANGE" => {
            setrange(store, args, out);
            Effect::Write
        }
        b"GETEX" => crate::set_options::getex(store, args, out),
        _ => return None,
    })
}

fn get<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return wrong_args(out, "get");
    }
    match store.get(&args[1]) {
        Ok(Some(v)) => encode_bulk(out, &v),
        Ok(None) => encode_null_bulk(out),
        Err(e) => store_err(out, e),
    }
}

/// `SETEX` / `PSETEX key ttl value`.
fn setex<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    unit_ms: i64,
    name: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 4 {
        wrong_args(out, name);
        return Effect::Unchanged;
    }
    let Some(n) = arg_i64(&args[2]).filter(|&n| n > 0) else {
        encode_error(out, &format!("ERR invalid expire time in '{name}' command"));
        return Effect::Unchanged;
    };
    let ms = n.saturating_mul(unit_ms) as u64;
    store.set_slice(
        &args[1],
        &args[3],
        Some(Duration::from_millis(ms)),
        kevy_store::SetCondition::Always,
    );
    encode_simple_string(out, "OK");
    Effect::Write
}

fn incr<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    delta: i64,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 2 {
        wrong_args(out, cmd);
    } else {
        emit_int_result(store.incr_by(&args[1], delta), out);
    }
    Effect::Write
}

fn incr_by<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    negate: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 3 {
        wrong_args(out, cmd);
        return Effect::Unchanged;
    }
    let Some(mut delta) = arg_i64(&args[2]) else {
        encode_error(out, ERR_NOT_INT);
        return Effect::Unchanged;
    };
    if negate {
        let Some(neg) = delta.checked_neg() else {
            encode_error(out, "ERR decrement would overflow");
            return Effect::Unchanged;
        };
        delta = neg;
    }
    emit_int_result(store.incr_by(&args[1], delta), out);
    Effect::Write
}

fn getdel<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() != 2 {
        wrong_args(out, "getdel");
        return Effect::Unchanged;
    }
    match store.getdel(&args[1]) {
        Ok(Some(v)) => {
            encode_bulk(out, &v);
            Effect::Write
        }
        Ok(None) => {
            encode_null_bulk(out);
            Effect::Unchanged
        }
        Err(e) => {
            store_err(out, e);
            Effect::Unchanged
        }
    }
}

/// `SETRANGE key offset value` — overwrite from `offset`, zero-padding
/// a short value out to it.
fn setrange<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 4 {
        return wrong_args(out, "setrange");
    }
    // Redis parses the offset first and refuses a non-integer in those
    // words; only then does a negative one get "offset is out of range"
    let Some(off) = arg_i64(&args[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    if off < 0 {
        return encode_error(out, "ERR offset is out of range");
    }
    emit_int_result(store.setrange(&args[1], off as u64, &args[3]).map(|n| n as i64), out);
}
