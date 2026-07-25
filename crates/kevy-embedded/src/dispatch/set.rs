//! Set-family verbs, including the read + `*STORE` algebra forms.

use crate::store::Store;
use crate::KevyResult;

use super::util::{
    arg_i64, bulk, emit_bulk_array, emit_int, err, kevy_err, nil, rest, wrong_args, ERR_NOT_INT,
};

/// One set-family request; `false` = verb not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per set verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"SADD" => {
            if argv.len() < 3 {
                wrong_args(out, "sadd");
            } else {
                emit_int(out, s.sadd(&argv[1], &rest(argv, 2)).map(|n| n as i64));
            }
        }
        b"SREM" => {
            if argv.len() < 3 {
                wrong_args(out, "srem");
            } else {
                emit_int(out, s.srem(&argv[1], &rest(argv, 2)).map(|n| n as i64));
            }
        }
        b"SCARD" => {
            if argv.len() == 2 {
                emit_int(out, s.scard(&argv[1]).map(|n| n as i64));
            } else {
                wrong_args(out, "scard");
            }
        }
        b"SISMEMBER" => {
            if argv.len() == 3 {
                emit_int(out, s.sismember(&argv[1], &argv[2]).map(i64::from));
            } else {
                wrong_args(out, "sismember");
            }
        }
        b"SMEMBERS" => {
            if argv.len() == 2 {
                emit_bulk_array(out, s.smembers(&argv[1]));
            } else {
                wrong_args(out, "smembers");
            }
        }
        b"SPOP" => cmd_spop_rand(s, argv, true, out),
        b"SRANDMEMBER" => cmd_spop_rand(s, argv, false, out),
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

/// `SPOP`/`SRANDMEMBER key [count]` — single reply without count,
/// array with it; a NEGATIVE `SRANDMEMBER` count samples with
/// repetition (composed from single draws over the typed facade).
fn cmd_spop_rand(s: &Store, argv: &[Vec<u8>], remove: bool, out: &mut Vec<u8>) {
    let name = if remove { "spop" } else { "srandmember" };
    if argv.len() < 2 || argv.len() > 3 {
        return wrong_args(out, name);
    }
    let count_given = argv.len() == 3;
    let raw = if count_given {
        match arg_i64(&argv[2]) {
            Some(c) => c,
            None => return err(out, ERR_NOT_INT),
        }
    } else {
        1
    };
    if raw < 0 && remove {
        return err(out, "ERR value is out of range, must be positive");
    }
    let count = raw.unsigned_abs() as usize;
    let res = if remove {
        s.spop(&argv[1], count)
    } else if raw < 0 {
        srandmember_with_repeats(s, &argv[1], count)
    } else {
        s.srandmember(&argv[1], count)
    };
    match res {
        Err(e) => kevy_err(out, &e),
        Ok(items) => {
            if count_given {
                emit_bulk_array(out, Ok(items));
            } else {
                match items.into_iter().next() {
                    Some(v) => bulk(out, &v),
                    None => nil(out),
                }
            }
        }
    }
}

/// Sample-with-replacement: `count` independent single draws.
fn srandmember_with_repeats(s: &Store, key: &[u8], count: usize) -> KevyResult<Vec<Vec<u8>>> {
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let mut one = s.srandmember(key, 1)?;
        match one.pop() {
            Some(m) => items.push(m),
            None => break, // empty set — nothing to repeat
        }
    }
    Ok(items)
}

/// The set-algebra op shapes, named so the dispatch helpers' signatures
/// stay readable (and clippy's type-complexity line stays green).
type AlgebraReadOp = fn(&Store, &[&[u8]]) -> KevyResult<Vec<Vec<u8>>>;
type AlgebraStoreOp = fn(&Store, &[u8], &[&[u8]]) -> KevyResult<usize>;

/// `SINTER`/`SUNION`/`SDIFF key [key …]`.
fn cmd_algebra_read(
    s: &Store,
    argv: &[Vec<u8>],
    out: &mut Vec<u8>,
    name: &str,
    op: AlgebraReadOp,
) {
    if argv.len() < 2 {
        return wrong_args(out, name);
    }
    emit_bulk_array(out, op(s, &rest(argv, 1)));
}

/// `S*STORE dst key [key …]` — replies with the stored cardinality.
/// Arity error mirrors the server's `parse_setstore_args` (a bare
/// "ERR wrong number of arguments", no verb interpolation).
fn cmd_algebra_store(
    s: &Store,
    argv: &[Vec<u8>],
    out: &mut Vec<u8>,
    op: AlgebraStoreOp,
) {
    if argv.len() < 3 {
        return err(out, "ERR wrong number of arguments");
    }
    emit_int(out, op(s, &argv[1], &rest(argv, 2)).map(|n| n as i64));
}
