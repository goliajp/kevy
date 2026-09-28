//! The list commands that name two keys (`RPOPLPUSH`, `BRPOPLPUSH`,
//! `LMOVE`), and `LPOS`. Both keys of a move must live in the store
//! passed in: routing them there is the caller's job.

use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
};
use kevy_store::{Store, StoreError};

use crate::Effect;
use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, ERR_SYNTAX, store_err, wrong_args};

/// One of these commands; `None` = the verb is not in this group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        b"RPOPLPUSH" => {
            if args.len() != 3 {
                wrong_args(out, "rpoplpush");
                return Some(Effect::Unchanged);
            }
            moved(store.rpoplpush(&args[1], &args[2]), true, out)
        }
        b"BRPOPLPUSH" => {
            if args.len() != 4 {
                wrong_args(out, "brpoplpush");
                return Some(Effect::Unchanged);
            }
            if !valid_timeout(&args[3]) {
                encode_error(out, "ERR timeout is not a float or out of range");
                return Some(Effect::Unchanged);
            }
            // an empty source writes nothing, so a caller that can block parks
            moved(store.rpoplpush(&args[1], &args[2]), false, out)
        }
        b"LMOVE" => lmove(store, args, out),
        b"LPOS" => {
            lpos(store, args, out);
            Effect::Read
        }
        _ => return None,
    })
}

/// A blocking timeout: a finite, non-negative number of seconds.
pub(crate) fn valid_timeout(b: &[u8]) -> bool {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .is_some_and(|f| f.is_finite() && f >= 0.0)
}

/// The reply to a pop-and-push: the moved element, or nothing to move
/// (a null bulk, or no reply at all for the blocking form).
fn moved(
    res: Result<Option<Vec<u8>>, StoreError>,
    null_when_empty: bool,
    out: &mut Vec<u8>,
) -> Effect {
    match res {
        Ok(Some(v)) => {
            encode_bulk(out, &v);
            Effect::Write
        }
        Ok(None) => {
            if null_when_empty {
                encode_null_bulk(out);
            }
            Effect::Unchanged
        }
        Err(e) => {
            store_err(out, e);
            Effect::Unchanged
        }
    }
}

fn side(b: &[u8]) -> Option<bool> {
    if b.eq_ignore_ascii_case(b"LEFT") {
        Some(true)
    } else if b.eq_ignore_ascii_case(b"RIGHT") {
        Some(false)
    } else {
        None
    }
}

/// `LMOVE source destination LEFT|RIGHT LEFT|RIGHT`.
fn lmove<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() != 5 {
        wrong_args(out, "lmove");
        return Effect::Unchanged;
    }
    let (Some(from), Some(to)) = (side(&args[3]), side(&args[4])) else {
        encode_error(out, ERR_SYNTAX);
        return Effect::Unchanged;
    };
    moved(store.lmove(&args[1], &args[2], from, to), true, out)
}

/// `LPOS key element [RANK n] [COUNT n] [MAXLEN n]`.
///
/// `RANK 1` (the default) is the first match from the head, `RANK -1`
/// the first from the tail. Without `COUNT` the reply is one index (or
/// a null bulk); with it, an array, where `COUNT 0` means every match.
/// `MAXLEN 0` scans the whole list.
fn lpos<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "lpos");
    }
    let Some((rank, count, maxlen)) = lpos_opts(args, out) else {
        return;
    };
    match store.lpos(&args[1], &args[2], rank, count, maxlen) {
        Err(e) => store_err(out, e),
        Ok(hits) => match count {
            None => match hits.first() {
                Some(idx) => encode_integer(out, *idx),
                None => encode_null_bulk(out),
            },
            Some(_) => {
                encode_array_len(out, hits.len() as i64);
                for idx in &hits {
                    encode_integer(out, *idx);
                }
            }
        },
    }
}

/// The `[RANK n] [COUNT n] [MAXLEN n]` tail: `(rank, count, maxlen)`,
/// or `None` with the refusal already in `out`.
fn lpos_opts<A: ArgvView + ?Sized>(
    args: &A,
    out: &mut Vec<u8>,
) -> Option<(i64, Option<i64>, usize)> {
    let mut rank: i64 = 1;
    let mut count: Option<i64> = None;
    let mut maxlen: usize = 0;
    let mut i = 3;
    while i < args.len() {
        let tok = &args[i];
        let known = [&b"RANK"[..], b"COUNT", b"MAXLEN"].iter().any(|k| tok.eq_ignore_ascii_case(k));
        if !known {
            encode_error(out, ERR_SYNTAX);
            return None;
        }
        let v = lpos_value(args, i, out)?;
        if tok.eq_ignore_ascii_case(b"RANK") {
            if v == 0 {
                encode_error(
                    out,
                    "ERR RANK can't be zero: use 1 to start from the first match going forward, or -1 from the last match going backward.",
                );
                return None;
            }
            rank = v;
        } else if tok.eq_ignore_ascii_case(b"COUNT") {
            if v < 0 {
                encode_error(out, "ERR COUNT can't be negative");
                return None;
            }
            count = Some(v);
        } else {
            if v < 0 {
                encode_error(out, "ERR MAXLEN can't be negative");
                return None;
            }
            maxlen = v as usize;
        }
        i += 2;
    }
    Some((rank, count, maxlen))
}

/// The integer after the option at `i`, or the refusal in `out`.
fn lpos_value<A: ArgvView + ?Sized>(args: &A, i: usize, out: &mut Vec<u8>) -> Option<i64> {
    if i + 1 >= args.len() {
        encode_error(out, ERR_SYNTAX);
        return None;
    }
    let v = arg_i64(&args[i + 1]);
    if v.is_none() {
        encode_error(out, ERR_NOT_INT);
    }
    v
}
