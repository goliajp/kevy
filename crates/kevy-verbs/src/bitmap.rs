//! Bit-level access to a string value: `GETBIT` / `SETBIT` /
//! `BITCOUNT` / `BITPOS`. Every refusal is worded as Redis words it.

use kevy_resp::{ArgvView, encode_error, encode_integer};
use kevy_store::{BitUnit, Store};

use crate::Effect;
use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, ERR_SYNTAX, emit_int_result, store_err, wrong_args};

/// An unsigned bit offset, read as a signed integer first so `-0` and
/// `+5` parse the way Redis parses them.
fn arg_u64<A: ArgvView + ?Sized>(args: &A, i: usize) -> Option<u64> {
    arg_i64(&args[i]).and_then(|n| u64::try_from(n).ok())
}

/// One bitmap command; `None` = the verb is not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per bitmap verb.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        b"GETBIT" => {
            if args.len() != 3 {
                wrong_args(out, "getbit");
            } else if let Some(off) = arg_u64(args, 2) {
                emit_int_result(store.getbit(&args[1], off).map(i64::from), out);
            } else {
                encode_error(out, "ERR bit offset is not an integer or out of range");
            }
            Effect::Read
        }
        b"SETBIT" => {
            setbit(store, args, out);
            Effect::Write
        }
        b"BITCOUNT" => {
            bitcount(store, args, out);
            Effect::Read
        }
        b"BITPOS" => {
            bitpos(store, args, out);
            Effect::Read
        }
        _ => return None,
    })
}

fn setbit<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 4 {
        return wrong_args(out, "setbit");
    }
    let Some(off) = arg_u64(args, 2) else {
        return encode_error(out, "ERR bit offset is not an integer or out of range");
    };
    let Some(v @ (0 | 1)) = arg_u64(args, 3) else {
        return encode_error(out, "ERR bit is not an integer or out of range");
    };
    emit_int_result(store.setbit(&args[1], off, v as u8).map(i64::from), out);
}

/// The `BYTE | BIT` unit at `args[i]`, `BYTE` when absent; `None` is a
/// syntax error.
fn unit<A: ArgvView + ?Sized>(args: &A, i: usize) -> Option<BitUnit> {
    match args.get(i) {
        None => Some(BitUnit::Byte),
        Some(u) if u.eq_ignore_ascii_case(b"BYTE") => Some(BitUnit::Byte),
        Some(u) if u.eq_ignore_ascii_case(b"BIT") => Some(BitUnit::Bit),
        Some(_) => None,
    }
}

/// `BITCOUNT key [start end [BYTE | BIT]]`. The range is read before the
/// key, so a bad number is refused whatever the key holds.
fn bitcount<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    let range = match args.len() {
        2 => None,
        4 | 5 => {
            let (Some(s), Some(e)) = (arg_i64(&args[2]), arg_i64(&args[3])) else {
                return encode_error(out, ERR_NOT_INT);
            };
            let Some(u) = unit(args, 4) else { return encode_error(out, ERR_SYNTAX) };
            Some((s, e, u))
        }
        0 | 1 => return wrong_args(out, "bitcount"),
        _ => return encode_error(out, ERR_SYNTAX),
    };
    emit_int_result(store.bitcount_in(&args[1], range).map(|n| n as i64), out);
}

/// `BITPOS key bit [start [end [BYTE | BIT]]]`, its arguments read before
/// the key as Redis reads them.
fn bitpos<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "bitpos");
    }
    if args.len() > 6 {
        return encode_error(out, ERR_SYNTAX);
    }
    let Some(bit @ (0 | 1)) = arg_u64(args, 2) else {
        return encode_error(out, "ERR The bit argument must be 1 or 0.");
    };
    // start, then the unit, then the end: the order Redis reads them in
    let num = |i: usize| args.get(i).map(|a| arg_i64(a).ok_or(())).transpose();
    let Ok(start) = num(3) else { return encode_error(out, ERR_NOT_INT) };
    let Some(u) = unit(args, 5) else { return encode_error(out, ERR_SYNTAX) };
    let Ok(end) = num(4) else { return encode_error(out, ERR_NOT_INT) };
    match store.bitpos_in(&args[1], bit as u8, start, end, u) {
        Ok(Some(pos)) => encode_integer(out, pos as i64),
        Ok(None) => encode_integer(out, -1),
        Err(e) => store_err(out, e),
    }
}
