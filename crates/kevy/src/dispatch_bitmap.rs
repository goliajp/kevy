//! Bit-level access to a string value: `GETBIT` / `SETBIT` /
//! `BITCOUNT` / `BITPOS`.
//!
//! One file per layer, same name at each: `kevy-store/src/bitmap.rs`
//! holds the engine, `kevy-embedded/src/dispatch/bitmap.rs` the
//! facade's table, and this one the server's. The wording of every
//! refusal below is Redis's, because the two surfaces are compared
//! byte for byte in `differential_wire_vs_embedded.rs` and Redis is
//! what both are being compatible with.

use crate::cmd::{ERR_NOT_INT, arg_i64, emit_int_result, store_err, wrong_args};
use kevy_resp::{ArgvView, encode_error, encode_integer};
use kevy_store::Store;

/// Parse an unsigned bit offset. Redis names the offset, not the type,
/// when it refuses.
fn arg_u64<A: ArgvView + ?Sized>(args: &A, i: usize) -> Option<u64> {
    arg_i64(&args[i]).and_then(|n| u64::try_from(n).ok())
}

/// One bitmap command; `false` = the verb is not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per bitmap verb.
pub(crate) fn dispatch_bitmap<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"GETBIT" => {
            if args.len() != 3 {
                wrong_args(out, "getbit");
            } else if let Some(off) = arg_u64(args, 2) {
                emit_int_result(store.getbit(&args[1], off).map(i64::from), out);
            } else {
                encode_error(out, "ERR bit offset is not an integer or out of range");
            }
        }
        b"SETBIT" => cmd_setbit(store, args, out),
        b"BITCOUNT" => match args.len() {
            2 => emit_int_result(store.bitcount(&args[1], None).map(|n| n as i64), out),
            4 => match (arg_i64(&args[2]), arg_i64(&args[3])) {
                (Some(a), Some(b)) => {
                    emit_int_result(store.bitcount(&args[1], Some((a, b))).map(|n| n as i64), out);
                }
                _ => encode_error(out, ERR_NOT_INT),
            },
            0 | 1 => wrong_args(out, "bitcount"),
            _ => encode_error(out, "ERR syntax error"),
        },
        b"BITPOS" => cmd_bitpos(store, args, out),
        _ => return false,
    }
    true
}

fn cmd_setbit<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
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
fn cmd_bitpos<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
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
