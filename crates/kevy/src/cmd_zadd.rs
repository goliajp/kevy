//! `ZADD` — flags parsing (Redis 6.2 `NX`/`XX`/`GT`/`LT`/`CH`/`INCR`)
//! + dispatch body. Split from `cmd.rs` (500-LOC rule).

use kevy_resp::CmdError;
use kevy_resp::{ArgvView, encode_bulk, encode_error, encode_null_bulk};
use kevy_store::{Store, ZaddFlags};

use crate::cmd::{arg_f64, emit_int_result, fmt_score, store_err, wrong_args};

/// Leading `ZADD` option tokens (Redis 6.2): `NX`/`XX`/`GT`/`LT`/`CH`/
/// `INCR`. Returns `(flags, incr, index of the first score)`.
fn parse_zadd_flags<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<(ZaddFlags, bool, usize), CmdError> {
    let mut f = ZaddFlags::default();
    let mut incr = false;
    let mut i = 2;
    while i < args.len() {
        let a = &args[i];
        if a.eq_ignore_ascii_case(b"NX") {
            f.nx = true;
        } else if a.eq_ignore_ascii_case(b"XX") {
            f.xx = true;
        } else if a.eq_ignore_ascii_case(b"GT") {
            f.gt = true;
        } else if a.eq_ignore_ascii_case(b"LT") {
            f.lt = true;
        } else if a.eq_ignore_ascii_case(b"CH") {
            f.ch = true;
        } else if a.eq_ignore_ascii_case(b"INCR") {
            incr = true;
        } else {
            break;
        }
        i += 1;
    }
    if !f.valid() {
        return Err(CmdError::Wire("ERR GT, LT, and/or NX options at the same time are not compatible"));
    }
    Ok((f, incr, i))
}

/// `ZADD key [NX|XX] [GT|LT] [CH] [INCR] score member [score member ...]`.
/// G4 (v1.25): borrowed-pair path; the no-flags form stays on the
/// original `zadd` fast path untouched.
pub(crate) fn cmd_zadd<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    let (flags, incr, first) = match parse_zadd_flags(args) {
        Ok(t) => t,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    if args.len() < first + 2 || !(args.len() - first).is_multiple_of(2) {
        return wrong_args(out, "zadd");
    }
    let mut pairs: Vec<(f64, &[u8])> = Vec::with_capacity((args.len() - first) / 2);
    let mut i = first;
    while i < args.len() {
        let Some(score) = arg_f64(&args[i]) else {
            return encode_error(out, "ERR value is not a valid float");
        };
        pairs.push((score, &args[i + 1]));
        i += 2;
    }
    if incr {
        if pairs.len() != 1 {
            return encode_error(out, "ERR INCR option supports a single increment-element pair");
        }
        return match store.zadd_incr(&args[1], pairs[0].0, pairs[0].1, flags) {
            Ok(Some(next)) => encode_bulk(out, &fmt_score(next)),
            Ok(None) => encode_null_bulk(out),
            Err(e) => store_err(out, e),
        };
    }
    if flags == ZaddFlags::default() {
        return emit_int_result(store.zadd(&args[1], &pairs).map(|n| n as i64), out);
    }
    match store.zadd_flags(&args[1], &pairs, flags) {
        Ok(rep) => {
            let n = if flags.ch { rep.changed } else { rep.added };
            emit_int_result(Ok(n as i64), out);
        }
        Err(e) => store_err(out, e),
    }
}
