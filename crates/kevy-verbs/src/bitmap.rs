//! Bit-level access to a string value: `GETBIT` / `SETBIT` /
//! `BITCOUNT` / `BITPOS`. Every refusal is worded as Redis words it.

use kevy_resp::{ArgvView, encode_error, encode_integer};
use kevy_store::Store;

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
            match args.len() {
                2 => emit_int_result(store.bitcount(&args[1], None).map(|n| n as i64), out),
                4 => match (arg_i64(&args[2]), arg_i64(&args[3])) {
                    (Some(a), Some(b)) => emit_int_result(
                        store.bitcount(&args[1], Some((a, b))).map(|n| n as i64),
                        out,
                    ),
                    _ => encode_error(out, ERR_NOT_INT),
                },
                0 | 1 => wrong_args(out, "bitcount"),
                _ => encode_error(out, ERR_SYNTAX),
            }
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

/// `BITPOS key bit [start [end]]`. A missing end means "to the end",
/// which is `-1` in the engine's range language.
fn bitpos<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if !(3..=5).contains(&args.len()) {
        return wrong_args(out, "bitpos");
    }
    let Some(bit @ (0 | 1)) = arg_u64(args, 2) else {
        return encode_error(out, "ERR The bit argument must be 1 or 0.");
    };
    let range = match args.len() {
        3 => None,
        4 => match arg_i64(&args[3]) {
            Some(a) => Some((a, -1)),
            None => return encode_error(out, ERR_NOT_INT),
        },
        _ => match (arg_i64(&args[3]), arg_i64(&args[4])) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => return encode_error(out, ERR_NOT_INT),
        },
    };
    match store.bitpos(&args[1], bit as u8, range) {
        Ok(Some(pos)) => encode_integer(out, pos as i64),
        Ok(None) => encode_integer(out, -1),
        Err(e) => store_err(out, e),
    }
}
