//! `BITOP`, which reads several keys and writes another, so spans
//! shards. The single-key bitmap verbs run through `shared`.

use crate::BitOp;
use crate::store::Store;

use super::emit_int;
use kevy_resp::encode_error;
use kevy_verbs::reply::{ERR_SYNTAX, wrong_args};

/// `BITOP`; `false` = some other verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    if up != b"BITOP" {
        return false;
    }
    cmd_bitop(s, argv, out);
    true
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
        _ => return encode_error(out, ERR_SYNTAX),
    };
    let srcs: Vec<&[u8]> = argv[3..].iter().map(Vec::as_slice).collect();
    if op == BitOp::Not && srcs.len() != 1 {
        return encode_error(out, "ERR BITOP NOT must be called with a single source key.");
    }
    emit_int(out, s.bitop(op, &argv[2], &srcs).map(|n| n as i64));
}
