//! Sorted-set range reads (by rank and by score, both directions), the
//! pops, and `ZSCAN`.

use kevy_resp::{ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error};
use kevy_store::{LexBound, Store, StoreError};

use crate::args::{arg_f64, arg_i64, scan_match};
use crate::reply::{
    ERR_NOT_FLOAT, ERR_NOT_INT, ERR_SYNTAX, fmt_score, scan_page, store_err, wrong_args,
};
use crate::{Effect, changed};

/// One range or pop command; `None` = the verb is not in this group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let v2 = RespVersion::V2;
    Some(match cmd {
        b"ZRANGE" | b"ZREVRANGE" | b"ZRANGEBYSCORE" | b"ZREVRANGEBYSCORE" | b"ZRANGEBYLEX"
        | b"ZREVRANGEBYLEX" => {
            crate::zrange::zrange(store, args, out, v2);
            Effect::Read
        }
        b"ZRANGESTORE" => crate::zrange::zrangestore(store, args, out),
        b"ZLEXCOUNT" => {
            lex(store, args, out, false);
            Effect::Read
        }
        b"ZREMRANGEBYLEX" => lex(store, args, out, true),
        b"ZPOPMIN.BELOW" => zpopmin_below(store, args, out),
        b"ZSCAN" => {
            zscan(store, args, out);
            Effect::Read
        }
        _ => return None,
    })
}

/// `ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT offset count]`, in
/// the reply shape of `proto` — [`crate::cmd::zrange`] under its older
/// name, which fixes the kind and direction.
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
    crate::zrange::zrange(store, args, out, proto);
}

/// `ZLEXCOUNT key min max`, or with `remove` `ZREMRANGEBYLEX key min max`.
fn lex<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    remove: bool,
) -> Effect {
    if args.len() != 4 {
        wrong_args(out, if remove { "zremrangebylex" } else { "zlexcount" });
        return Effect::Unchanged;
    }
    let (Some(min), Some(max)) = (LexBound::parse(&args[2]), LexBound::parse(&args[3])) else {
        encode_error(out, "ERR min or max not valid string range item");
        return Effect::Unchanged;
    };
    let n = if remove {
        store.zremrange_by_lex(&args[1], &min, &max)
    } else {
        store.zlexcount(&args[1], &min, &max)
    };
    match n {
        Ok(n) => {
            kevy_resp::encode_integer(out, n as i64);
            changed(remove && n > 0)
        }
        Err(e) => {
            store_err(out, e);
            Effect::Unchanged
        }
    }
}

/// The optional pop count at `i`: 1 when absent, the refusal otherwise.
fn pop_count<A: ArgvView + ?Sized>(args: &A, i: usize, out: &mut Vec<u8>) -> Option<usize> {
    if args.len() <= i {
        return Some(1);
    }
    let Some(c) = arg_i64(&args[i]) else {
        encode_error(out, "ERR value is out of range, must be positive");
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
