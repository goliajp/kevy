//! Sorted-set range reads (by rank and by score, both directions), the
//! pops, and `ZSCAN`.

use kevy_resp::{ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error};
use kevy_store::{Store, StoreError};

use crate::args::{arg_f64, arg_i64, parse_score_bound, scan_match};
use crate::reply::{
    ERR_NOT_FLOAT, ERR_NOT_INT, ERR_SYNTAX, emit_zrange, fmt_score, scan_page, store_err,
    wrong_args,
};
use crate::{Effect, changed, list_move};

const ERR_MIN_MAX: &str = "ERR min or max is not a float";

/// One range or pop command; `None` = the verb is not in this group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let v2 = RespVersion::V2;
    Some(match cmd {
        b"ZRANGE" => {
            zrange(store, args, out, v2);
            Effect::Read
        }
        b"ZREVRANGE" => {
            by_rank(store, args, out, v2, true);
            Effect::Read
        }
        b"ZRANGEBYSCORE" => {
            zrangebyscore(store, args, out, v2);
            Effect::Read
        }
        b"ZREVRANGEBYSCORE" => {
            by_score(store, args, out, v2, true);
            Effect::Read
        }
        b"ZPOPMIN" => zpopmin(store, args, out),
        b"ZPOPMIN.BELOW" => zpopmin_below(store, args, out),
        b"BZPOPMIN" => bzpopmin(store, args, out),
        b"ZSCAN" => {
            zscan(store, args, out);
            Effect::Read
        }
        _ => return None,
    })
}

/// `ZRANGE key start stop [WITHSCORES]`, by rank, in the reply shape of
/// `proto`.
///
/// ```
/// use kevy_resp::{Argv, RespVersion};
/// let mut store = kevy_store::Store::new();
/// store.zadd(b"z", &[(1.0, &b"a"[..])]).unwrap();
/// let argv = Argv::from(vec![b"ZRANGE".to_vec(), b"z".to_vec(), b"0".to_vec(), b"-1".to_vec()]);
/// let mut out = Vec::new();
/// kevy_verbs::cmd::zrange(&mut store, &argv, &mut out, RespVersion::V2);
/// assert_eq!(out, b"*1\r\n$1\r\na\r\n");
/// ```
pub fn zrange<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    by_rank(store, args, out, proto, false);
}

/// `ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT offset count]`, in
/// the reply shape of `proto`. `WITHSCORES` and `LIMIT` may come in
/// either order, each at most once.
///
/// ```
/// use kevy_resp::{Argv, RespVersion};
/// let mut store = kevy_store::Store::new();
/// store.zadd(b"z", &[(1.0, &b"a"[..]), (5.0, &b"b"[..])]).unwrap();
/// let argv = Argv::from(vec![b"ZRANGEBYSCORE".to_vec(), b"z".to_vec(), b"(1".to_vec(), b"+inf".to_vec()]);
/// let mut out = Vec::new();
/// kevy_verbs::cmd::zrangebyscore(&mut store, &argv, &mut out, RespVersion::V2);
/// assert_eq!(out, b"*1\r\n$1\r\nb\r\n");
/// ```
pub fn zrangebyscore<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    by_score(store, args, out, proto, false);
}

/// `ZRANGE` / `ZREVRANGE key start stop [WITHSCORES]`.
fn by_rank<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
    rev: bool,
) {
    if args.len() < 4 || args.len() > 5 {
        return wrong_args(out, if rev { "zrevrange" } else { "zrange" });
    }
    let withscores = args.len() == 5;
    if withscores && !args[4].eq_ignore_ascii_case(b"WITHSCORES") {
        return encode_error(out, ERR_SYNTAX);
    }
    let (Some(start), Some(stop)) = (arg_i64(&args[2]), arg_i64(&args[3])) else {
        return encode_error(out, ERR_NOT_INT);
    };
    let res = if rev {
        store.zrevrange(&args[1], start, stop)
    } else {
        store.zrange(&args[1], start, stop)
    };
    emit_zrange(res, withscores, proto, out);
}

/// `ZRANGEBYSCORE key min max …` / `ZREVRANGEBYSCORE key max min …`:
/// the reverse form names its bounds high first, as Redis does.
fn by_score<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
    rev: bool,
) {
    if args.len() < 4 {
        return wrong_args(out, if rev { "zrevrangebyscore" } else { "zrangebyscore" });
    }
    let (lo, hi) = if rev { (3, 2) } else { (2, 3) };
    let (Some(min), Some(max)) = (parse_score_bound(&args[lo]), parse_score_bound(&args[hi]))
    else {
        return encode_error(out, ERR_MIN_MAX);
    };
    let Some((withscores, limit)) = range_modifiers(args, out) else {
        return;
    };
    let res = if rev {
        store.zrev_range_by_score(&args[1], min, max)
    } else {
        store.zrange_by_score(&args[1], min, max)
    };
    match res {
        Err(e) => store_err(out, e),
        Ok(mut items) => {
            if let Some((off, cnt)) = limit {
                let start = off.max(0) as usize;
                if start >= items.len() {
                    items.clear();
                } else if cnt < 0 {
                    // a negative count is everything after the offset
                    items.drain(..start);
                } else {
                    let end = (start + cnt as usize).min(items.len());
                    items = items[start..end].to_vec();
                }
            }
            emit_zrange(Ok(items), withscores, proto, out);
        }
    }
}

/// The `[WITHSCORES] [LIMIT offset count]` tail, in either order, each
/// at most once. `None` = the refusal is already in `out`.
fn range_modifiers<A: ArgvView + ?Sized>(
    args: &A,
    out: &mut Vec<u8>,
) -> Option<(bool, Option<(i64, i64)>)> {
    let mut withscores = false;
    let mut limit: Option<(i64, i64)> = None;
    let mut i = 4;
    while i < args.len() {
        let tok = &args[i];
        if tok.eq_ignore_ascii_case(b"WITHSCORES") {
            if withscores {
                encode_error(out, ERR_SYNTAX);
                return None;
            }
            withscores = true;
            i += 1;
        } else if tok.eq_ignore_ascii_case(b"LIMIT") {
            if limit.is_some() || i + 2 >= args.len() {
                encode_error(out, ERR_SYNTAX);
                return None;
            }
            let (Some(off), Some(cnt)) = (arg_i64(&args[i + 1]), arg_i64(&args[i + 2])) else {
                encode_error(out, ERR_NOT_INT);
                return None;
            };
            limit = Some((off, cnt));
            i += 3;
        } else {
            encode_error(out, ERR_SYNTAX);
            return None;
        }
    }
    Some((withscores, limit))
}

/// The optional pop count at `i`: 1 when absent, the refusal otherwise.
fn pop_count<A: ArgvView + ?Sized>(args: &A, i: usize, out: &mut Vec<u8>) -> Option<usize> {
    if args.len() <= i {
        return Some(1);
    }
    let Some(c) = arg_i64(&args[i]) else {
        encode_error(out, ERR_NOT_INT);
        return None;
    };
    if c < 0 {
        encode_error(out, "ERR value is out of range, must be positive");
        return None;
    }
    Some(c as usize)
}

/// Popped `(member, score)` pairs as the flat `[m, s, …]` reply.
fn popped(res: Result<Vec<(Vec<u8>, f64)>, StoreError>, out: &mut Vec<u8>) -> Effect {
    match res {
        Err(e) => {
            store_err(out, e);
            Effect::Unchanged
        }
        Ok(items) => {
            encode_array_len(out, (items.len() * 2) as i64);
            for (m, sc) in &items {
                encode_bulk(out, m);
                encode_bulk(out, &fmt_score(*sc));
            }
            changed(!items.is_empty())
        }
    }
}

/// `ZPOPMIN key [count]`.
fn zpopmin<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() < 2 || args.len() > 3 {
        wrong_args(out, "zpopmin");
        return Effect::Unchanged;
    }
    let Some(count) = pop_count(args, 2, out) else {
        return Effect::Unchanged;
    };
    popped(store.zpopmin(&args[1], count), out)
}

/// `ZPOPMIN.BELOW key below [count]` — pop the lowest members scored
/// strictly under `below`: with the score as a due time and `below` as
/// now, "take what is due" in one step.
fn zpopmin_below<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() < 3 || args.len() > 4 {
        wrong_args(out, "zpopmin.below");
        return Effect::Unchanged;
    }
    let Some(below) = arg_f64(&args[2]) else {
        encode_error(out, ERR_NOT_FLOAT);
        return Effect::Unchanged;
    };
    let Some(count) = pop_count(args, 3, out) else {
        return Effect::Unchanged;
    };
    popped(store.zpopmin_below(&args[1], below, count), out)
}

/// `BZPOPMIN key [key …] timeout`. One key with members pops one and
/// answers `[key, member, score]`; otherwise nothing is written and a
/// caller that can block parks the connection, as with `BLPOP`.
fn bzpopmin<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() < 3 {
        wrong_args(out, "bzpopmin");
        return Effect::Unchanged;
    }
    if !list_move::valid_timeout(&args[args.len() - 1]) {
        encode_error(out, "ERR timeout is not a float or out of range");
        return Effect::Unchanged;
    }
    if args.len() > 3 {
        return Effect::Unchanged;
    }
    match store.zpopmin(&args[1], 1) {
        Err(e) => store_err(out, e),
        Ok(items) => {
            if let Some((member, score)) = items.into_iter().next() {
                encode_array_len(out, 3);
                encode_bulk(out, &args[1]);
                encode_bulk(out, &member);
                encode_bulk(out, &fmt_score(score));
                return Effect::Write;
            }
        }
    }
    Effect::Unchanged
}

/// `ZSCAN key cursor [MATCH pattern] [COUNT n]` — every member in one
/// batch, member then score.
fn zscan<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "zscan");
    }
    if arg_i64(&args[2]).is_none() {
        return encode_error(out, ERR_NOT_INT);
    }
    let Some(pat) = scan_match(args, 3) else {
        return encode_error(out, ERR_SYNTAX);
    };
    match store.zrange(&args[1], 0, -1) {
        Err(e) => store_err(out, e),
        Ok(items) => {
            let mut page: Vec<Vec<u8>> = Vec::with_capacity(items.len() * 2);
            for (m, sc) in items {
                if pat.as_ref().is_none_or(|p| kevy_store::glob_match(p, &m)) {
                    page.push(m);
                    page.push(fmt_score(sc));
                }
            }
            scan_page(out, &page);
        }
    }
}
