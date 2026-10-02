//! zset algebra `*STORE` forms + `ZINTERCARD`, parsed by the grammar the
//! server uses (`kevy_verbs::multikey`).

use crate::KevyResult;
use crate::store::Store;

use kevy_store::ZAggregate;

use super::{Args, emit_int, verb_name};
use kevy_resp::encode_error;
use kevy_verbs::multikey::{parse_zdiffstore, parse_zintercard, parse_zstore};
use kevy_verbs::reply::wrong_args;

/// One zset-algebra request; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"ZINTERSTORE" => cmd_zstore(s, argv, out, false, Store::zinterstore),
        b"ZUNIONSTORE" => cmd_zstore(s, argv, out, false, Store::zunionstore),
        b"ZDIFFSTORE" => {
            cmd_zstore(s, argv, out, true, |s, dst, keys, _w, _a| s.zdiffstore(dst, keys))
        }
        b"ZINTERCARD" => cmd_zintercard(s, argv, out),
        _ => return false,
    }
    true
}

type ZStoreOp = fn(&Store, &[u8], &[&[u8]], Option<&[f64]>, ZAggregate) -> KevyResult<usize>;

/// `VERB dst numkeys key… [WEIGHTS w…] [AGGREGATE SUM|MIN|MAX]`
/// (`diff_form` = ZDIFFSTORE: no WEIGHTS/AGGREGATE allowed).
fn cmd_zstore(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>, diff_form: bool, op: ZStoreOp) {
    if argv.len() < 4 {
        return wrong_args(out, &verb_name(argv));
    }
    let args = Args::new(argv);
    let parsed = if diff_form { parse_zdiffstore(&args) } else { parse_zstore(&args) };
    let z = match parsed {
        Ok(z) => z,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let keys: Vec<&[u8]> = argv[3..3 + z.numkeys].iter().map(Vec::as_slice).collect();
    emit_int(out, op(s, &argv[1], &keys, z.weights.as_deref(), z.aggregate).map(|n| n as i64));
}

/// `ZINTERCARD numkeys key… [LIMIT n]` — `limit = 0` means unlimited.
fn cmd_zintercard(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 3 {
        return wrong_args(out, &verb_name(argv));
    }
    let (numkeys, limit) = match parse_zintercard(&Args::new(argv)) {
        Ok(t) => t,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let keys: Vec<&[u8]> = argv[2..2 + numkeys].iter().map(Vec::as_slice).collect();
    emit_int(out, s.zintercard(&keys, limit).map(|n| n as i64));
}
