//! `BITOP`, which reads several keys and writes another, so spans
//! shards. The single-key bitmap verbs run through `shared`.

use crate::store::Store;

use super::{Args, emit_int};
use kevy_resp::encode_error;

/// `BITOP`; `false` = some other verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    if up != b"BITOP" {
        return false;
    }
    cmd_bitop(s, argv, out);
    true
}

fn cmd_bitop(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let op = match kevy_verbs::multikey::parse_bitop(&Args::new(argv)) {
        Ok(op) => op,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let srcs: Vec<&[u8]> = argv[3..].iter().map(Vec::as_slice).collect();
    emit_int(out, s.bitop(op, &argv[2], &srcs).map(|n| n as i64));
}
