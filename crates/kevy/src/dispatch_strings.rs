//! String commands, and the byte-level access to a string value that
//! Redis gives them: `GETRANGE` / `SETRANGE` beside `GET` / `SET`.
//!
//! Split out of [`crate::dispatch`], which had grown to within thirty
//! lines of the 500-LOC house rule with the router, the connection
//! table, the set table and the generic table already in it. The layout
//! now matches the embedded facade's (`kevy-embedded/src/dispatch/
//! strings.rs`), which matters more than it sounds: the two surfaces are
//! compared verb by verb in `differential_wire_vs_embedded.rs`, and a
//! reader checking one against the other should not have to hold two
//! different file layouts in their head.

use crate::cmd::{
    ERR_NOT_INT, arg_f64, arg_i64, cmd_incr, cmd_incr_by, cmd_setex, emit_int_result, store_err,
    wrong_args,
};
use kevy_resp::{ArgvView, encode_bulk, encode_error, encode_integer, encode_null_bulk};
use kevy_store::Store;

/// String commands.
// LOC-WAIVER: data-driven verb dispatch table — one arm per string verb.
pub(crate) fn dispatch_string<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        // No GET or SET arm, on purpose. `dispatch_with_proto` answers
        // both in its tier-1 fast path and RETURNS before the handler
        // chain is walked, so arms here could never run — and the GET
        // one was a verbatim second copy of the fast path's, which is
        // the kind of duplicate that drifts silently because neither
        // half can be observed disagreeing with the other. `deadgate`
        // is what found it: thirteen never-executed regions in a
        // function every request walks.
        b"APPEND" => {
            if args.len() == 3 {
                emit_int_result(store.append(&args[1], &args[2]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "append");
            }
        }
        b"STRLEN" => {
            if args.len() == 2 {
                emit_int_result(store.strlen(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "strlen");
            }
        }
        b"INCR" => cmd_incr(store, args, 1, "incr", out),
        b"DECR" => cmd_incr(store, args, -1, "decr", out),
        b"INCRBY" => cmd_incr_by(store, args, false, "incrby", out),
        b"DECRBY" => cmd_incr_by(store, args, true, "decrby", out),
        b"SETNX" => {
            if args.len() == 3 {
                let set = store.set_slice(&args[1], &args[2], None, true, false);
                encode_integer(out, i64::from(set));
            } else {
                wrong_args(out, "setnx");
            }
        }
        b"SETEX" => cmd_setex(store, args, 1000, "setex", out),
        b"PSETEX" => cmd_setex(store, args, 1, "psetex", out),
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
        }
        b"GETDEL" => {
            if args.len() == 2 {
                match store.getdel(&args[1]) {
                    Ok(Some(v)) => encode_bulk(out, &v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                wrong_args(out, "getdel");
            }
        }
        b"INCRBYFLOAT" => {
            if args.len() != 3 {
                wrong_args(out, "incrbyfloat");
            } else if let Some(d) = arg_f64(&args[2]) {
                match store.incr_by_float(&args[1], d) {
                    Ok(v) => encode_bulk(out, &v),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, "ERR value is not a valid float");
            }
        }
        b"GETRANGE" => {
            if args.len() != 4 {
                wrong_args(out, "getrange");
            } else if let (Some(a), Some(b)) = (arg_i64(&args[2]), arg_i64(&args[3])) {
                match store.getrange(&args[1], a, b) {
                    Ok(v) => encode_bulk(out, &v),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"SETRANGE" => cmd_setrange(store, args, out),
        b"GETEX" => cmd_getex(store, args, out),
        _ => return false,
    }
    true
}

/// `SETRANGE key offset value` — overwrite from `offset`, zero-padding
/// a short value out to it. Redis caps the resulting string at 512 MB
/// and says so in those words.
fn cmd_setrange<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 4 {
        return wrong_args(out, "setrange");
    }
    // Two refusals, not one. Redis parses the offset first — a
    // non-integer is "value is not an integer or out of range" — and
    // only then rejects a negative one as "offset is out of range".
    // Folding them into a single `filter(|n| n >= 0)` answered the
    // second sentence to the first question; the differential against
    // the facade said so on the run that first drove `SETRANGE k abc x`.
    let Some(off) = arg_i64(&args[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    if off < 0 {
        return encode_error(out, "ERR offset is out of range");
    }
    emit_int_result(store.setrange(&args[1], off as u64, &args[3]).map(|n| n as i64), out);
}

/// `GETEX key [EX seconds | PX milliseconds]` — read, and set the
/// deadline in the same call. The bare form is a plain GET, which is
/// why Redis classifies the verb as a write regardless: the argv, not
/// the verb, decides whether a deadline moves.
fn cmd_getex<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    match args.len() {
        2 => match store.get(&args[1]) {
            Ok(Some(v)) => encode_bulk(out, &v),
            Ok(None) => encode_null_bulk(out),
            Err(e) => store_err(out, e),
        },
        4 => {
            let ex = args[2].eq_ignore_ascii_case(b"EX");
            if !ex && !args[2].eq_ignore_ascii_case(b"PX") {
                return encode_error(out, "ERR syntax error");
            }
            let Some(n) = arg_i64(&args[3]).filter(|&n| n > 0) else {
                return encode_error(out, "ERR invalid expire time in 'getex' command");
            };
            let ms = if ex { n.saturating_mul(1000) } else { n };
            // Read first, and only move the deadline when there was a
            // value to read — `Store::expire` on a missing key would
            // answer false, but asking it at all is a write the reply
            // does not account for.
            match store.get(&args[1]) {
                Ok(Some(v)) => {
                    let v = v.to_vec();
                    store.expire(&args[1], std::time::Duration::from_millis(ms as u64));
                    encode_bulk(out, &v);
                }
                Ok(None) => encode_null_bulk(out),
                Err(e) => store_err(out, e),
            }
        }
        0 | 1 => wrong_args(out, "getex"),
        _ => encode_error(out, "ERR syntax error"),
    }
}
