//! The sorted-set pops, ranks and picks — `ZPOPMIN`, `ZPOPMAX`, `ZRANK`,
//! `ZREVRANK`, `ZMSCORE`, `ZRANDMEMBER` — each written once for both
//! protocols: RESP3 sends scores as doubles and an absent value as `_`.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_double, encode_error,
    encode_integer, encode_null, encode_null_bulk,
};
use kevy_store::{Store, ZSpan};

use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, ERR_SYNTAX, store_err, wrong_args};
use crate::{Effect, changed};

const ERR_POSITIVE: &str = "ERR value is out of range, must be positive";

/// One command of this group over RESP2; `None` = not in the group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let v2 = RespVersion::V2;
    Some(match cmd {
        b"ZPOPMIN" => zpopmin(store, args, out, v2),
        b"ZPOPMAX" => zpopmax(store, args, out, v2),
        b"ZRANK" => zrank(store, args, out, v2),
        b"ZREVRANK" => zrevrank(store, args, out, v2),
        b"ZMSCORE" => zmscore(store, args, out, v2),
        b"ZRANDMEMBER" => zrandmember(store, args, out, v2),
        b"BZPOPMIN" => bzpopmin(store, args, out, v2),
        b"BZPOPMAX" => bzpopmax(store, args, out, v2),
        _ => return None,
    })
}

pub(crate) fn score(out: &mut Vec<u8>, s: f64, proto: RespVersion) {
    match proto {
        RespVersion::V3 => encode_double(out, s),
        _ => kevy_resp::encode_bulk_double(out, s),
    }
}

/// A pop recorded as the removal of what it took: `ZREM` is in every
/// version, so an older reader of the log or a replica can follow it, and
/// a replay cannot take different members.
pub(crate) fn zrem_record(key: &[u8], taken: &[(Vec<u8>, f64)]) -> Effect {
    let mut frame = Vec::with_capacity(taken.len() + 2);
    frame.push(b"ZREM".to_vec());
    frame.push(key.to_vec());
    frame.extend(taken.iter().map(|(m, _)| m.clone()));
    Effect::Record(frame)
}

fn absent(out: &mut Vec<u8>, proto: RespVersion, array: bool) {
    match (proto, array) {
        (RespVersion::V3, _) => encode_null(out),
        (_, true) => out.extend_from_slice(b"*-1\r\n"),
        _ => encode_null_bulk(out),
    }
}

/// `ZPOPMIN key [count]`.
pub fn zpopmin<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    zpop(store, args, out, false, proto)
}

/// `ZPOPMAX key [count]`.
pub fn zpopmax<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    zpop(store, args, out, true, proto)
}

fn zpop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    max: bool,
    proto: RespVersion,
) -> Effect {
    if args.len() < 2 {
        wrong_args(out, if max { "zpopmax" } else { "zpopmin" });
        return Effect::Unchanged;
    }
    if args.len() > 3 {
        encode_error(out, ERR_SYNTAX);
        return Effect::Unchanged;
    }
    let count = match args.get(2).map(arg_i64) {
        None => 1,
        Some(Some(c)) if c >= 0 => c as usize,
        Some(_) => {
            encode_error(out, ERR_POSITIVE);
            return Effect::Unchanged;
        }
    };
    pop_into(store, &args[1], count, max, proto, out)
}

/// Pop up to `count` from `key` straight into the reply. ZPOPMAX is newer
/// than some readers of the log, so it is recorded as the ZREM of what it
/// took, and that record owns the members.
fn pop_into(
    store: &mut Store,
    key: &[u8],
    count: usize,
    max: bool,
    proto: RespVersion,
    out: &mut Vec<u8>,
) -> Effect {
    let mut frame: Vec<Vec<u8>> = Vec::new();
    let start = out.len();
    let taken = store.zpop_each(key, count, max, |m, s| {
        encode_bulk(out, m);
        score(out, s, proto);
        if max {
            frame.push(m.to_vec());
        }
    });
    let n = match taken {
        Ok(n) => n,
        Err(e) => {
            store_err(out, e);
            return Effect::Unchanged;
        }
    };
    let body = out.len();
    encode_array_len(out, (n * 2) as i64);
    let head = out.len() - body;
    out[start..].rotate_right(head);
    if max && n > 0 {
        frame.splice(0..0, [b"ZREM".to_vec(), key.to_vec()]);
        Effect::Record(frame)
    } else {
        changed(n > 0)
    }
}

/// `BZPOPMIN key [key …] timeout`. One key with members pops one and
/// answers `[key, member, score]`; otherwise nothing is written and a
/// caller that can block parks the connection, as with `BLPOP`.
pub fn bzpopmin<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    bzpop(store, args, out, false, proto)
}

/// `BZPOPMAX key [key …] timeout`: `BZPOPMIN` from the top.
pub fn bzpopmax<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    bzpop(store, args, out, true, proto)
}

fn bzpop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    max: bool,
    proto: RespVersion,
) -> Effect {
    if args.len() < 3 {
        wrong_args(out, if max { "bzpopmax" } else { "bzpopmin" });
        return Effect::Unchanged;
    }
    if let Some(e) = crate::list_move::timeout_refusal(&args[args.len() - 1]) {
        encode_error(out, e);
        return Effect::Unchanged;
    }
    if args.len() > 3 {
        return Effect::Unchanged;
    }
    let res = if max { store.zpopmax(&args[1], 1) } else { store.zpopmin(&args[1], 1) };
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => {
            if let Some((member, s)) = items.first() {
                encode_array_len(out, 3);
                encode_bulk(out, &args[1]);
                encode_bulk(out, member);
                score(out, *s, proto);
                return zrem_record(&args[1], &items);
            }
        }
    }
    Effect::Unchanged
}

/// `ZRANK key member [WITHSCORE]`.
pub fn zrank<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    rank(store, args, out, false, proto)
}

/// `ZREVRANK key member [WITHSCORE]`.
pub fn zrevrank<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    rank(store, args, out, true, proto)
}

fn rank<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    rev: bool,
    proto: RespVersion,
) -> Effect {
    if args.len() < 3 {
        wrong_args(out, if rev { "zrevrank" } else { "zrank" });
        return Effect::Read;
    }
    let withscore = match args.get(3) {
        None => false,
        Some(a) if args.len() == 4 && a.eq_ignore_ascii_case(b"WITHSCORE") => true,
        Some(_) => {
            encode_error(out, ERR_SYNTAX);
            return Effect::Read;
        }
    };
    let (key, member) = (&args[1], &args[2]);
    let rank = if rev { store.zrevrank(key, member) } else { store.zrank(key, member) };
    match rank {
        Err(e) => store_err(out, e),
        Ok(None) => absent(out, proto, withscore),
        Ok(Some(r)) if withscore => {
            let s = store.zscore(key, member).ok().flatten().unwrap_or_default();
            encode_array_len(out, 2);
            encode_integer(out, r as i64);
            score(out, s, proto);
        }
        Ok(Some(r)) => encode_integer(out, r as i64),
    }
    Effect::Read
}

/// `ZMSCORE key member [member …]`.
pub fn zmscore<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    if args.len() < 3 {
        wrong_args(out, "zmscore");
        return Effect::Read;
    }
    // the key's type is the only error, and the first lookup meets it
    let first = match store.zscore(&args[1], &args[2]) {
        Ok(s) => s,
        Err(e) => {
            store_err(out, e);
            return Effect::Read;
        }
    };
    encode_array_len(out, (args.len() - 2) as i64);
    let emit = |out: &mut Vec<u8>, s: Option<f64>| match s {
        Some(s) => score(out, s, proto),
        None => absent(out, proto, false),
    };
    emit(out, first);
    for i in 3..args.len() {
        emit(out, store.zscore(&args[1], &args[i]).unwrap_or(None));
    }
    Effect::Read
}

/// `ZRANDMEMBER key [count [WITHSCORES]]`.
pub fn zrandmember<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    if args.len() < 2 {
        wrong_args(out, "zrandmember");
        return Effect::Read;
    }
    if args.len() > 4 || (args.len() == 4 && !args[3].eq_ignore_ascii_case(b"WITHSCORES")) {
        encode_error(out, ERR_SYNTAX);
        return Effect::Read;
    }
    let count = match args.get(2).map(arg_i64) {
        None => None,
        Some(Some(i64::MIN)) => {
            encode_error(
                out,
                "ERR value is out of range, value must between -9223372036854775807 and 9223372036854775807",
            );
            return Effect::Read;
        }
        Some(Some(c)) => Some(c),
        Some(None) => {
            encode_error(out, ERR_NOT_INT);
            return Effect::Read;
        }
    };
    let Some(count) = count else {
        match store.zrandmember_rank(&args[1]) {
            Ok(Some(r)) => encode_bulk(out, at_rank(store, &args[1], r).0),
            Ok(None) => absent(out, proto, false),
            Err(e) => store_err(out, e),
        }
        return Effect::Read;
    };
    match store.zrandmember_ranks(&args[1], count) {
        Ok(ranks) => emit_picked(store, &args[1], &ranks, args.len() == 4, proto, out),
        Err(e) => store_err(out, e),
    }
    Effect::Read
}

/// The member at a rank just picked from `key`, read in place. Nothing
/// can move between the pick and the read; were it gone, an empty member
/// keeps the reply whole.
fn at_rank<'s>(store: &'s mut Store, key: &[u8], r: usize) -> (&'s [u8], f64) {
    let one = store.zrange_select(key, ZSpan::Rank(r as i64, r as i64), false, None);
    one.ok().and_then(|mut one| one.next()).unwrap_or((&[], 0.0))
}

/// The members at `ranks`, read in place one by one.
fn emit_picked(
    store: &mut Store,
    key: &[u8],
    ranks: &[usize],
    withscores: bool,
    proto: RespVersion,
    out: &mut Vec<u8>,
) {
    let nested = withscores && proto == RespVersion::V3;
    let per = if withscores && !nested { 2 } else { 1 };
    encode_array_len(out, (ranks.len() * per) as i64);
    for &r in ranks {
        let (m, s) = at_rank(store, key, r);
        if nested {
            encode_array_len(out, 2);
        }
        encode_bulk(out, m);
        if withscores {
            score(out, s, proto);
        }
    }
}
