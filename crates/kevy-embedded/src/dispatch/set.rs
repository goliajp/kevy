//! The set algebra, which reads or writes several keys and so spans
//! shards: SINTER / SUNION / SDIFF and their `STORE` forms. The
//! single-key set verbs run through `shared`.

use crate::KevyResult;
use crate::store::Store;

use super::{emit_bulk_array, emit_int, rest, verb_name};
use kevy_verbs::reply::wrong_args;

/// One set-algebra request; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"SINTER" => cmd_algebra_read(s, argv, out, "sinter", Store::sinter),
        b"SUNION" => cmd_algebra_read(s, argv, out, "sunion", Store::sunion),
        b"SDIFF" => cmd_algebra_read(s, argv, out, "sdiff", Store::sdiff),
        b"SINTERSTORE" => cmd_algebra_store(s, argv, out, Store::sinterstore),
        b"SUNIONSTORE" => cmd_algebra_store(s, argv, out, Store::sunionstore),
        b"SDIFFSTORE" => cmd_algebra_store(s, argv, out, Store::sdiffstore),
        _ => return false,
    }
    true
}

/// The set-algebra op shapes, named so the dispatch helpers' signatures
/// stay readable (and clippy's type-complexity line stays green).
type AlgebraReadOp = fn(&Store, &[&[u8]]) -> KevyResult<Vec<Vec<u8>>>;
type AlgebraStoreOp = fn(&Store, &[u8], &[&[u8]]) -> KevyResult<usize>;

/// `SINTER`/`SUNION`/`SDIFF key [key …]`.
fn cmd_algebra_read(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>, name: &str, op: AlgebraReadOp) {
    if argv.len() < 2 {
        return wrong_args(out, name);
    }
    emit_bulk_array(out, op(s, &rest(argv, 1)));
}

/// `S*STORE dst key [key …]` — replies with the stored cardinality.
///
/// The arity error used to mirror `kevy-rt`'s `parse_setstore_args`, a bare
/// "ERR wrong number of arguments" with no verb interpolation. That mirror
/// was aimed at the wrong reflection: a short S*STORE never reaches that
/// parser, because `cmd_resolve` guards the route with
/// `if args.len() >= 3` and a shorter call falls through to the dispatch
/// chain, which names the verb the way Redis does. The wire differential
/// found twelve verbs split this way.
fn cmd_algebra_store(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>, op: AlgebraStoreOp) {
    if argv.len() < 3 {
        return wrong_args(out, &verb_name(argv));
    }
    emit_int(out, op(s, &argv[1], &rest(argv, 2)).map(|n| n as i64));
}
