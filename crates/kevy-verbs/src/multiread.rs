//! Reads over several keys that answer from one computation: `ZINTER` /
//! `ZUNION` / `ZDIFF`, `SINTERCARD`, `LCS`. Every key named must live in
//! the store passed in — a server whose keys span shards gathers copies of
//! them into one first.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error, encode_integer,
};
use kevy_store::{Store, StoreError};

use crate::Effect;
use crate::lcs::lcs_reply;
use crate::multikey::{parse_zcombine, parse_zdiff, parse_zintercard};
use crate::reply::{store_err, wrong_args};
use crate::zset_pick::score;

/// One command of this group over RESP2; `None` = not in the group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let v2 = RespVersion::V2;
    match cmd {
        b"ZINTER" | b"ZUNION" | b"ZDIFF" => zcombine(store, args, out, v2),
        b"SINTERCARD" => sintercard(store, args, out),
        b"LCS" => lcs(store, args, out, v2),
        _ => return None,
    }
    Some(Effect::Read)
}

/// `ZINTER` / `ZUNION` / `ZDIFF numkeys key… […] [WITHSCORES]`: the
/// combination, ordered as a sorted set orders its members.
pub fn zcombine<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 3 {
        return wrong_args(out, &String::from_utf8_lossy(&args[0]).to_ascii_lowercase());
    }
    let diff = args[0].eq_ignore_ascii_case(b"ZDIFF");
    let parsed = if diff { parse_zdiff(args) } else { parse_zcombine(args) };
    let p = match parsed {
        Ok(p) => p,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let mut inputs = Vec::with_capacity(p.numkeys);
    for i in 2..2 + p.numkeys {
        match store.zset_or_set_members(&args[i]) {
            Ok(m) => inputs.push(m),
            Err(e) => return store_err(out, e),
        }
    }
    let mut result = if diff {
        kevy_store::zdiff(&inputs)
    } else if args[0].eq_ignore_ascii_case(b"ZINTER") {
        kevy_store::zinter(&inputs, p.weights.as_deref(), p.aggregate)
    } else {
        kevy_store::zunion(&inputs, p.weights.as_deref(), p.aggregate)
    };
    for (_, s) in &mut result {
        // an inf minus an inf has no order to sort by; Redis calls it 0
        if s.is_nan() {
            *s = 0.0;
        }
    }
    result.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    emit_scored(out, &result, p.withscores, proto);
}

fn emit_scored(out: &mut Vec<u8>, items: &[(Vec<u8>, f64)], withscores: bool, proto: RespVersion) {
    let nested = withscores && proto == RespVersion::V3;
    let per = if withscores && !nested { 2 } else { 1 };
    encode_array_len(out, (items.len() * per) as i64);
    for (m, s) in items {
        if nested {
            encode_array_len(out, 2);
        }
        encode_bulk(out, m);
        if withscores {
            score(out, *s, proto);
        }
    }
}

/// `SINTERCARD numkeys key… [LIMIT n]`: the size of the intersection,
/// counting no further than `n` (0 = all). A missing key answers 0 at
/// once, before the keys after it are looked at, as Redis does.
fn sintercard<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "sintercard");
    }
    let (numkeys, limit) = match parse_zintercard(args) {
        Ok(p) => p,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let mut sets = Vec::with_capacity(numkeys);
    for i in 2..2 + numkeys {
        match store.set_snapshot(&args[i]) {
            Err(e) => return store_err(out, e),
            Ok(m) if m.is_empty() => return encode_integer(out, 0),
            Ok(m) => sets.push(m),
        }
    }
    sets.sort_by_key(Vec::len);
    let (first, rest) = sets.split_first().map_or((&[][..], &[][..]), |(f, r)| (&f[..], r));
    let others: Vec<std::collections::HashSet<&[u8]>> =
        rest.iter().map(|s| s.iter().map(Vec::as_slice).collect()).collect();
    let mut n = 0usize;
    for m in first {
        if others.iter().all(|o| o.contains(m.as_slice())) {
            n += 1;
            if n == limit {
                break;
            }
        }
    }
    encode_integer(out, n as i64);
}

/// `LCS key1 key2 [LEN] [IDX] [MINMATCHLEN n] [WITHMATCHLEN]`.
pub fn lcs<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 3 {
        return wrong_args(out, "lcs");
    }
    let mut strings = [Vec::new(), Vec::new()];
    for (i, s) in strings.iter_mut().enumerate() {
        match store.get(&args[1 + i]) {
            Ok(v) => *s = v.map(|c| c.into_owned()).unwrap_or_default(),
            Err(StoreError::WrongType) => {
                return encode_error(out, "ERR The specified keys must contain string values");
            }
            Err(e) => return store_err(out, e),
        }
    }
    lcs_reply(args, &strings[0], &strings[1], out, proto);
}
