//! List commands. The two-key moves and `LPOS` live in `list_move`.

use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_null_bulk, encode_simple_string,
};
use kevy_store::{InsertPosition, Store};

use crate::args::{arg_i64, with_rest};
use crate::reply::{
    ERR_NOT_INT, ERR_SYNTAX, emit_bulk_array, emit_int_result, store_err, wrong_args,
};
use crate::{Effect, changed, list_move};

/// One list command; `None` = the verb is not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per list verb.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        b"LPUSH" | b"RPUSH" => {
            if args.len() < 3 {
                wrong_args(out, if cmd == b"LPUSH" { "lpush" } else { "rpush" });
            } else {
                let res = with_rest(args, 2, |vals| {
                    if cmd == b"LPUSH" {
                        store.lpush(&args[1], vals)
                    } else {
                        store.rpush(&args[1], vals)
                    }
                });
                emit_int_result(res.map(|n| n as i64), out);
            }
            Effect::Write
        }
        b"LPUSHX" | b"RPUSHX" => {
            if args.len() < 3 {
                wrong_args(out, if cmd == b"LPUSHX" { "lpushx" } else { "rpushx" });
                return Some(Effect::Unchanged);
            }
            let res = with_rest(args, 2, |vals| {
                if cmd == b"LPUSHX" {
                    store.lpushx(&args[1], vals)
                } else {
                    store.rpushx(&args[1], vals)
                }
            });
            // a missing key takes nothing
            let pushed = matches!(res, Ok(n) if n > 0);
            emit_int_result(res.map(|n| n as i64), out);
            changed(pushed)
        }
        b"LPOP" => pop(store, args, false, out),
        b"RPOP" => pop(store, args, true, out),
        b"BLPOP" => blocking_pop(store, args, false, out),
        b"BRPOP" => blocking_pop(store, args, true, out),
        b"LLEN" => {
            if args.len() == 2 {
                emit_int_result(store.llen(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "llen");
            }
            Effect::Read
        }
        b"LINDEX" => {
            if args.len() != 3 {
                wrong_args(out, "lindex");
            } else if let Some(i) = arg_i64(&args[2]) {
                match store.lindex(&args[1], i) {
                    Ok(Some(v)) => encode_bulk(out, &v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
            Effect::Read
        }
        b"LRANGE" => {
            if args.len() != 4 {
                wrong_args(out, "lrange");
            } else if let (Some(s), Some(e)) = (arg_i64(&args[2]), arg_i64(&args[3])) {
                emit_bulk_array(store.lrange(&args[1], s, e), out);
            } else {
                encode_error(out, ERR_NOT_INT);
            }
            Effect::Read
        }
        b"LSET" => {
            if args.len() != 4 {
                wrong_args(out, "lset");
            } else if let Some(i) = arg_i64(&args[2]) {
                match store.lset(&args[1], i, &args[3]) {
                    Ok(()) => encode_simple_string(out, "OK"),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
            Effect::Write
        }
        b"LINSERT" => linsert(store, args, out),
        b"LREM" => {
            if args.len() != 4 {
                wrong_args(out, "lrem");
                return Some(Effect::Unchanged);
            }
            let Some(c) = arg_i64(&args[2]) else {
                encode_error(out, ERR_NOT_INT);
                return Some(Effect::Unchanged);
            };
            let res = store.lrem(&args[1], c, &args[3]);
            let removed = matches!(res, Ok(n) if n > 0);
            emit_int_result(res.map(|n| n as i64), out);
            changed(removed)
        }
        b"LTRIM" => {
            if args.len() != 4 {
                wrong_args(out, "ltrim");
            } else if let (Some(s), Some(e)) = (arg_i64(&args[2]), arg_i64(&args[3])) {
                match store.ltrim(&args[1], s, e) {
                    Ok(()) => encode_simple_string(out, "OK"),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
            Effect::Write
        }
        b"RPOPLPUSH" | b"BRPOPLPUSH" | b"LMOVE" | b"BLMOVE" | b"LPOS" => {
            return list_move::exec(cmd, store, args, out);
        }
        _ => return None,
    })
}

/// `LPOP` / `RPOP key [count]` — a single bulk without a count, an
/// array (or a null array when there is nothing) with one.
fn pop<A: ArgvView + ?Sized>(store: &mut Store, args: &A, tail: bool, out: &mut Vec<u8>) -> Effect {
    if args.len() < 2 || args.len() > 3 {
        wrong_args(out, if tail { "rpop" } else { "lpop" });
        return Effect::Unchanged;
    }
    let count_given = args.len() == 3;
    let count = if count_given {
        match arg_i64(&args[2]) {
            Some(c) if c >= 0 => c as usize,
            _ => {
                encode_error(out, "ERR value is out of range, must be positive");
                return Effect::Unchanged;
            }
        }
    } else {
        1
    };
    // a count of 0 takes nothing: an empty array for a list, nil for none
    if count_given && count == 0 {
        match store.llen(&args[1]) {
            Ok(0) => encode_array_len(out, -1),
            Ok(_) => encode_array_len(out, 0),
            Err(e) => store_err(out, e),
        }
        return Effect::Unchanged;
    }
    let res = if tail { store.rpop(&args[1], count) } else { store.lpop(&args[1], count) };
    let items = match res {
        Ok(items) => items,
        Err(e) => {
            store_err(out, e);
            return Effect::Unchanged;
        }
    };
    if !count_given {
        match items.first() {
            Some(v) => encode_bulk(out, v),
            None => encode_null_bulk(out),
        }
    } else if items.is_empty() {
        encode_array_len(out, -1);
    } else {
        encode_array_len(out, items.len() as i64);
        for it in &items {
            encode_bulk(out, it);
        }
    }
    changed(!items.is_empty())
}

/// `BLPOP` / `BRPOP key [key …] timeout`.
///
/// With one key and a non-empty list this pops and answers
/// `[key, value]`, recorded as the `LPOP` / `RPOP` it performed.
/// Otherwise it changes nothing and asks for no record of its own: a
/// caller that can block parks the connection on the key(s) without
/// recording anything, so a record asked for here would be left for the
/// next write on the thread to take. The single-key form is what it
/// replays when a push wakes it. The timeout is still checked first, so
/// a malformed one is refused rather than blocked on.
fn blocking_pop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    tail: bool,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 3 {
        wrong_args(out, if tail { "brpop" } else { "blpop" });
        return Effect::Unchanged;
    }
    if let Some(e) = list_move::timeout_refusal(&args[args.len() - 1]) {
        encode_error(out, e);
        return Effect::Unchanged;
    }
    if args.len() > 3 {
        return Effect::Unchanged;
    }
    let res = if tail { store.rpop(&args[1], 1) } else { store.lpop(&args[1], 1) };
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => {
            if let Some(v) = items.into_iter().next() {
                encode_array_len(out, 2);
                encode_bulk(out, &args[1]);
                encode_bulk(out, &v);
                let pop: &[u8] = if tail { b"RPOP" } else { b"LPOP" };
                return Effect::Record(vec![pop.to_vec(), args[1].to_vec()]);
            }
        }
    }
    Effect::Unchanged
}

/// `LINSERT key BEFORE|AFTER pivot value`: the new length, `0` for a
/// missing key, `-1` for a missing pivot.
fn linsert<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() != 5 {
        wrong_args(out, "linsert");
        return Effect::Unchanged;
    }
    let position = if args[2].eq_ignore_ascii_case(b"BEFORE") {
        InsertPosition::Before
    } else if args[2].eq_ignore_ascii_case(b"AFTER") {
        InsertPosition::After
    } else {
        encode_error(out, ERR_SYNTAX);
        return Effect::Unchanged;
    };
    let res = store.linsert(&args[1], position, &args[3], &args[4]);
    let inserted = matches!(res, Ok(n) if n > 0);
    emit_int_result(res, out);
    changed(inserted)
}
