//! Bitmap verbs (string-backed): GETBIT / SETBIT / BITCOUNT / BITPOS /
//! BITOP. Server-side these are a ledgered RESP gap (F3), so the
//! wording follows Redis.

use crate::store::Store;
use crate::BitOp;

use super::util::{arg_i64, arg_u64, emit_int, err, kevy_err, wrong_args, ERR_NOT_INT, ERR_SYNTAX};

/// One bitmap request; `false` = verb not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per bitmap verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"GETBIT" => {
            if argv.len() != 3 {
                wrong_args(out, "getbit");
            } else if let Some(off) = arg_u64(&argv[2]) {
                emit_int(out, s.getbit(&argv[1], off).map(i64::from));
            } else {
                err(out, "ERR bit offset is not an integer or out of range");
            }
        }
        b"SETBIT" => cmd_setbit(s, argv, out),
        b"BITCOUNT" => match argv.len() {
            2 => emit_int(out, s.bitcount(&argv[1], None).map(|n| n as i64)),
            4 => match (arg_i64(&argv[2]), arg_i64(&argv[3])) {
                (Some(a), Some(b)) => {
                    emit_int(out, s.bitcount(&argv[1], Some((a, b))).map(|n| n as i64));
                }
                _ => err(out, ERR_NOT_INT),
            },
            0 | 1 => wrong_args(out, "bitcount"),
            _ => err(out, ERR_SYNTAX),
        },
        b"BITPOS" => cmd_bitpos(s, argv, out),
        b"BITOP" => cmd_bitop(s, argv, out),
        _ => return false,
    }
    true
}

fn cmd_setbit(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() != 4 {
        return wrong_args(out, "setbit");
    }
    let Some(off) = arg_u64(&argv[2]) else {
        return err(out, "ERR bit offset is not an integer or out of range");
    };
    let Some(v @ (0 | 1)) = arg_u64(&argv[3]) else {
        return err(out, "ERR bit is not an integer or out of range");
    };
    emit_int(out, s.setbit(&argv[1], off, v as u8).map(i64::from));
}

fn cmd_bitpos(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if !(3..=5).contains(&argv.len()) {
        return wrong_args(out, "bitpos");
    }
    let Some(bit @ (0 | 1)) = arg_u64(&argv[2]) else {
        return err(out, "ERR The bit argument must be 1 or 0.");
    };
    let range = match argv.len() {
        3 => None,
        4 => match arg_i64(&argv[3]) {
            Some(a) => Some((a, -1)),
            None => return err(out, ERR_NOT_INT),
        },
        _ => match (arg_i64(&argv[3]), arg_i64(&argv[4])) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => return err(out, ERR_NOT_INT),
        },
    };
    match s.bitpos(&argv[1], bit as u8, range) {
        Ok(Some(pos)) => emit_int(out, Ok(pos as i64)),
        Ok(None) => emit_int(out, Ok(-1)),
        Err(e) => kevy_err(out, &e),
    }
}

fn cmd_bitop(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 4 {
        return wrong_args(out, "bitop");
    }
    let op = match argv[1].to_ascii_uppercase().as_slice() {
        b"AND" => BitOp::And,
        b"OR" => BitOp::Or,
        b"XOR" => BitOp::Xor,
        b"NOT" => BitOp::Not,
        _ => return err(out, ERR_SYNTAX),
    };
    let srcs: Vec<&[u8]> = argv[3..].iter().map(Vec::as_slice).collect();
    if op == BitOp::Not && srcs.len() != 1 {
        return err(out, "ERR BITOP NOT must be called with a single source key.");
    }
    emit_int(out, s.bitop(op, &argv[2], &srcs).map(|n| n as i64));
}
