//! The sorted-set range reads, all one query: `ZRANGE key start stop
//! [BYSCORE|BYLEX] [REV] [LIMIT offset count] [WITHSCORES]`, of which
//! `ZREVRANGE`, `ZRANGEBYSCORE`, `ZREVRANGEBYSCORE`, `ZRANGEBYLEX` and
//! `ZREVRANGEBYLEX` are the older spellings with the kind and direction
//! fixed, and `ZRANGESTORE` the form that keeps the result.

use kevy_resp::{ArgvView, RespVersion, encode_error};
use kevy_store::{LexBound, Store, StoreError};

use crate::args::{arg_i64, parse_score_bound};
use crate::reply::{ERR_NOT_INT, ERR_SYNTAX, Scores, emit_zrange, store_err, wrong_args};

/// What the range bounds name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum By {
    Rank,
    Score,
    Lex,
}

/// A parsed range: what its bounds name, which way it reads, and what it
/// keeps of the result.
#[derive(Clone, Copy)]
pub(crate) struct Spec {
    by: By,
    rev: bool,
    limit: Option<(i64, i64)>,
    withscores: bool,
}

/// A refusal before the store is touched, or the store's own.
pub(crate) enum RangeError {
    Wire(&'static str),
    Store(StoreError),
}

/// The kind and direction a verb fixes, `None` where its arguments say.
fn preset(verb: &[u8]) -> (Option<By>, Option<bool>) {
    let upper = verb.to_ascii_uppercase();
    match upper.as_slice() {
        b"ZREVRANGE" => (Some(By::Rank), Some(true)),
        b"ZRANGEBYSCORE" => (Some(By::Score), Some(false)),
        b"ZREVRANGEBYSCORE" => (Some(By::Score), Some(true)),
        b"ZRANGEBYLEX" => (Some(By::Lex), Some(false)),
        b"ZREVRANGEBYLEX" => (Some(By::Lex), Some(true)),
        _ => (None, None),
    }
}

/// The options from argument `from` on, under the kind and direction
/// `verb` fixes; `store` forms take no WITHSCORES.
pub(crate) fn parse<A: ArgvView + ?Sized>(
    args: &A,
    from: usize,
    store: bool,
) -> Result<Spec, &'static str> {
    let (by_fixed, rev_fixed) = preset(&args[0]);
    let mut spec = Spec {
        by: by_fixed.unwrap_or(By::Rank),
        rev: rev_fixed.unwrap_or(false),
        limit: None,
        withscores: false,
    };
    let mut chose_by = false;
    let mut i = from;
    while i < args.len() {
        let a = &args[i];
        if !store && a.eq_ignore_ascii_case(b"WITHSCORES") {
            spec.withscores = true;
        } else if a.eq_ignore_ascii_case(b"LIMIT") && i + 2 < args.len() {
            let (Some(off), Some(cnt)) = (arg_i64(&args[i + 1]), arg_i64(&args[i + 2])) else {
                return Err(ERR_NOT_INT);
            };
            spec.limit = Some((off, cnt));
            i += 2;
        } else if by_fixed.is_none() && !chose_by && a.eq_ignore_ascii_case(b"BYSCORE") {
            (spec.by, chose_by) = (By::Score, true);
        } else if by_fixed.is_none() && !chose_by && a.eq_ignore_ascii_case(b"BYLEX") {
            (spec.by, chose_by) = (By::Lex, true);
        } else if rev_fixed.is_none() && a.eq_ignore_ascii_case(b"REV") {
            spec.rev = true;
        } else {
            return Err(ERR_SYNTAX);
        }
        i += 1;
    }
    if spec.limit.is_some() && spec.by == By::Rank {
        return Err(
            "ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX",
        );
    }
    if spec.withscores && spec.by == By::Lex {
        return Err("ERR syntax error, WITHSCORES not supported in combination with BYLEX");
    }
    Ok(spec)
}

/// The members `spec` selects from `key` between `start` and `stop` (a
/// reverse range names its high bound first), in reading order.
pub(crate) fn query(
    store: &mut Store,
    key: &[u8],
    start: &[u8],
    stop: &[u8],
    spec: Spec,
) -> Result<Vec<(Vec<u8>, f64)>, RangeError> {
    let (lo, hi) = if spec.rev { (stop, start) } else { (start, stop) };
    let items = match spec.by {
        By::Rank => {
            let (Some(a), Some(b)) = (arg_i64(start), arg_i64(stop)) else {
                return Err(RangeError::Wire(ERR_NOT_INT));
            };
            let r = if spec.rev { store.zrevrange(key, a, b) } else { store.zrange(key, a, b) };
            return r.map_err(RangeError::Store);
        }
        By::Score => {
            let (Some(min), Some(max)) = (parse_score_bound(lo), parse_score_bound(hi)) else {
                return Err(RangeError::Wire("ERR min or max is not a float"));
            };
            if spec.rev {
                store.zrev_range_by_score(key, min, max)
            } else {
                store.zrange_by_score(key, min, max)
            }
        }
        By::Lex => {
            let (Some(min), Some(max)) = (LexBound::parse(lo), LexBound::parse(hi)) else {
                return Err(RangeError::Wire("ERR min or max not valid string range item"));
            };
            store.zrange_by_lex(key, &min, &max).map(|mut v| {
                if spec.rev {
                    v.reverse();
                }
                v
            })
        }
    };
    let mut items = items.map_err(RangeError::Store)?;
    if let Some(limit) = spec.limit {
        apply_limit(&mut items, limit);
    }
    Ok(items)
}

/// Keep `count` items from `offset` on: a negative offset keeps nothing,
/// a negative count everything after the offset.
fn apply_limit(items: &mut Vec<(Vec<u8>, f64)>, (off, cnt): (i64, i64)) {
    if off < 0 || off as usize >= items.len() {
        items.clear();
    } else {
        items.drain(..off as usize);
        if cnt >= 0 {
            items.truncate(cnt as usize);
        }
    }
}

/// Any of the six range reads, in the reply shape of `proto`.
///
/// ```
/// use kevy_resp::{Argv, RespVersion};
/// let mut store = kevy_store::Store::new();
/// store.zadd(b"z", &[(1.0, &b"a"[..]), (2.0, &b"b"[..])]).unwrap();
/// let argv = Argv::from(["ZRANGE", "z", "+inf", "-inf", "BYSCORE", "REV", "LIMIT", "0", "1"].map(|s| s.as_bytes().to_vec()).to_vec());
/// let mut out = Vec::new();
/// kevy_verbs::cmd::zrange(&mut store, &argv, &mut out, RespVersion::V2);
/// assert_eq!(out, b"*1\r\n$1\r\nb\r\n");
/// ```
pub fn zrange<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 4 {
        return wrong_args(out, &String::from_utf8_lossy(&args[0]).to_ascii_lowercase());
    }
    let spec = match parse(args, 4, false) {
        Ok(s) => s,
        Err(e) => return encode_error(out, e),
    };
    let scores = if spec.withscores { Scores::Included } else { Scores::Omitted };
    match query(store, &args[1], &args[2], &args[3], spec) {
        Ok(items) => emit_zrange(Ok(items), scores, proto, out),
        Err(RangeError::Wire(e)) => encode_error(out, e),
        Err(RangeError::Store(e)) => store_err(out, e),
    }
}

/// `ZRANGESTORE dst src min max [BYSCORE|BYLEX] [REV] [LIMIT offset
/// count]`: the range of `src` replaces `dst`, whatever `dst` held; an
/// empty range deletes `dst`. How many members were stored.
///
/// ```
/// use kevy_resp::Argv;
/// let mut store = kevy_store::Store::new();
/// store.zadd(b"z", &[(1.0, &b"a"[..]), (2.0, &b"b"[..])]).unwrap();
/// let argv = Argv::from(["ZRANGESTORE", "d", "z", "0", "0"].map(|s| s.as_bytes().to_vec()).to_vec());
/// let mut out = Vec::new();
/// kevy_verbs::exec(&mut store, b"ZRANGESTORE", &argv, &mut out);
/// assert_eq!(out, b":1\r\n");
/// assert_eq!(store.zrange(b"d", 0, -1).unwrap(), [(b"a".to_vec(), 1.0)]);
/// ```
pub(crate) fn zrangestore<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> crate::Effect {
    if args.len() < 5 {
        wrong_args(out, "zrangestore");
        return crate::Effect::Unchanged;
    }
    let spec = match parse(args, 5, true) {
        Ok(s) => s,
        Err(e) => {
            encode_error(out, e);
            return crate::Effect::Unchanged;
        }
    };
    let items = match query(store, &args[2], &args[3], &args[4], spec) {
        Ok(items) => items,
        Err(RangeError::Wire(e)) => {
            encode_error(out, e);
            return crate::Effect::Unchanged;
        }
        Err(RangeError::Store(e)) => {
            store_err(out, e);
            return crate::Effect::Unchanged;
        }
    };
    let had = store.key_exists(&args[1]);
    let n = store.zstore_result(&args[1], &items);
    kevy_resp::encode_integer(out, n as i64);
    crate::changed(n > 0 || had)
}
