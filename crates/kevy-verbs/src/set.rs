//! Set commands on one key. The multi-key algebra (`SINTER`, `SUNION`,
//! `SDIFF` and their `STORE` forms) gathers across shards and is not
//! here.

use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_null_bulk};
use kevy_store::Store;

use crate::args::{arg_i64, rest_borrowed};
use crate::reply::{ERR_NOT_INT, emit_bulk_array, emit_int_result, store_err, wrong_args};
use crate::{Effect, changed};

/// One set command; `None` = the verb is not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per set verb.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        b"SADD" => {
            if args.len() < 3 {
                wrong_args(out, "sadd");
            } else {
                emit_int_result(
                    store.sadd(&args[1], &rest_borrowed(args, 2)).map(|n| n as i64),
                    out,
                );
            }
            Effect::Write
        }
        b"SREM" => {
            if args.len() < 3 {
                wrong_args(out, "srem");
                return Some(Effect::Unchanged);
            }
            let res = store.srem(&args[1], &rest_borrowed(args, 2));
            let removed = matches!(res, Ok(n) if n > 0);
            emit_int_result(res.map(|n| n as i64), out);
            changed(removed)
        }
        b"SMOVE" => smove(store, args, out),
        b"SMISMEMBER" => {
            if args.len() < 3 {
                wrong_args(out, "smismember");
                return Some(Effect::Read);
            }
            let mut flags = Vec::with_capacity(args.len() - 2);
            for i in 2..args.len() {
                match store.sismember(&args[1], &args[i]) {
                    Ok(b) => flags.push(b),
                    Err(e) => {
                        crate::reply::store_err(out, e);
                        return Some(Effect::Read);
                    }
                }
            }
            encode_array_len(out, flags.len() as i64);
            for b in flags {
                kevy_resp::encode_integer(out, i64::from(b));
            }
            Effect::Read
        }
        b"SCARD" => {
            if args.len() == 2 {
                emit_int_result(store.scard(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "scard");
            }
            Effect::Read
        }
        b"SISMEMBER" => {
            if args.len() == 3 {
                emit_int_result(store.sismember(&args[1], &args[2]).map(i64::from), out);
            } else {
                wrong_args(out, "sismember");
            }
            Effect::Read
        }
        b"SMEMBERS" => {
            if args.len() == 2 {
                emit_bulk_array(store.smembers(&args[1]), out);
            } else {
                wrong_args(out, "smembers");
            }
            Effect::Read
        }
        b"SPOP" => spop_rand(store, args, true, out),
        b"SRANDMEMBER" => spop_rand(store, args, false, out),
        b"SSCAN" => {
            crate::collection_scan::scan(store, args, crate::collection_scan::Kind::Set, out);
            Effect::Read
        }
        _ => return None,
    })
}

/// `SPOP` / `SRANDMEMBER key [count]` — a single reply without a count,
/// an array with one. A negative `SRANDMEMBER` count samples with
/// replacement; `SPOP` has no such form, since a member cannot be
/// removed twice.
///
/// `SPOP` is recorded as what it removed (`SREM key member…`), not as
/// the verb: replaying the verb would draw different members.
fn spop_rand<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    remove: bool,
    out: &mut Vec<u8>,
) -> Effect {
    let Some(raw) = spop_count(args, remove, out) else {
        return Effect::Unchanged;
    };
    let count_given = args.len() == 3;
    let count = raw.unsigned_abs() as usize;
    let res = if remove {
        store.spop(&args[1], count)
    } else if raw < 0 {
        store.srandmember_with_repeats(&args[1], count)
    } else {
        store.srandmember(&args[1], count)
    };
    let items = match res {
        Ok(items) => items,
        Err(e) => {
            store_err(out, e);
            return Effect::Unchanged;
        }
    };
    if count_given {
        encode_array_len(out, items.len() as i64);
        for it in &items {
            encode_bulk(out, it);
        }
    } else {
        match items.first() {
            Some(v) => encode_bulk(out, v),
            None => encode_null_bulk(out),
        }
    }
    if !remove {
        Effect::Read
    } else if items.is_empty() {
        Effect::Skip
    } else {
        let frame = crate::aof::spop_effect(&args[1], &items);
        Effect::Record(frame.into_iter().map(<[u8]>::to_vec).collect())
    }
}

/// The arity check and the optional count (1 when absent), or `None`
/// with the refusal in `out`.
fn spop_count<A: ArgvView + ?Sized>(args: &A, remove: bool, out: &mut Vec<u8>) -> Option<i64> {
    if args.len() < 2 || args.len() > 3 {
        wrong_args(out, if remove { "spop" } else { "srandmember" });
        return None;
    }
    let raw = if args.len() == 3 {
        let Some(c) = arg_i64(&args[2]) else {
            encode_error(out, ERR_NOT_INT);
            return None;
        };
        c
    } else {
        1
    };
    if raw < 0 && remove {
        encode_error(out, "ERR value is out of range, must be positive");
        return None;
    }
    Some(raw)
}

/// `SMOVE src dst member`, both keys in this store, in Redis's order of
/// checks: a missing source answers 0 whatever `dst` is; then each key's
/// type; then the member.
fn smove<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() != 4 {
        wrong_args(out, "smove");
        return Effect::Unchanged;
    }
    let (src, dst, member) = (&args[1], &args[2], &args[3]);
    let moved = match (store.scard(src), store.scard(dst)) {
        (Ok(0), _) => Ok(false),
        (Err(e), _) | (_, Err(e)) => Err(e),
        _ if src == dst => store.sismember(src, member),
        _ => store.srem(src, &[member]).and_then(|n| {
            if n == 0 {
                return Ok(false);
            }
            store.sadd(dst, &[member]).map(|_| true)
        }),
    };
    emit_int_result(moved.map(i64::from), out);
    // a key moved onto itself changes nothing
    changed(matches!(moved, Ok(true)) && src != dst)
}
