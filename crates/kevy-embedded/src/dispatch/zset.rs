//! Sorted-set verbs: ZADD (full 6.2 flag grammar), range reads,
//! rank/score reads, pops, remove-ranges and ZSCAN. The `*STORE`
//! algebra forms live in `zset_algebra.rs`.

use crate::store::Store;

use kevy_store::{ScoreBound, ZaddFlags};

use super::util::{
    arg_f64, arg_i64, bulk, emit_int, emit_scored, err, fmt_score, int, kevy_err, nil, rest,
    wrong_args, ERR_NOT_FLOAT, ERR_NOT_INT, ERR_SYNTAX,
};

const ERR_MIN_MAX: &str = "ERR min or max is not a float";

/// One zset request; `false` = verb not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per zset verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"ZADD" => cmd_zadd(s, argv, out),
        b"ZSCORE" => {
            if argv.len() == 3 {
                match s.zscore(&argv[1], &argv[2]) {
                    Ok(Some(sc)) => bulk(out, &fmt_score(sc)),
                    Ok(None) => nil(out),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "zscore");
            }
        }
        b"ZCARD" => {
            if argv.len() == 2 {
                emit_int(out, s.zcard(&argv[1]).map(|n| n as i64));
            } else {
                wrong_args(out, "zcard");
            }
        }
        b"ZREM" => {
            if argv.len() < 3 {
                wrong_args(out, "zrem");
            } else {
                emit_int(out, s.zrem(&argv[1], &rest(argv, 2)).map(|n| n as i64));
            }
        }
        b"ZRANK" => {
            if argv.len() == 3 {
                match s.zrank(&argv[1], &argv[2]) {
                    Ok(Some(r)) => int(out, r as i64),
                    Ok(None) => nil(out),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "zrank");
            }
        }
        b"ZINCRBY" => {
            if argv.len() != 4 {
                wrong_args(out, "zincrby");
            } else if let Some(d) = arg_f64(&argv[2]) {
                match s.zincrby(&argv[1], d, &argv[3]) {
                    Ok(sc) => bulk(out, &fmt_score(sc)),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                err(out, ERR_NOT_FLOAT);
            }
        }
        b"ZCOUNT" => cmd_zcount(s, argv, out),
        b"ZRANGE" => cmd_zrange(s, argv, out, false),
        b"ZREVRANGE" => cmd_zrange(s, argv, out, true),
        b"ZRANGEBYSCORE" => cmd_zrangebyscore(s, argv, out, false),
        b"ZREVRANGEBYSCORE" => cmd_zrangebyscore(s, argv, out, true),
        b"ZPOPMIN" => cmd_zpopmin(s, argv, out),
        b"ZPOPMIN.BELOW" => cmd_zpopmin_below(s, argv, out),
        b"ZREMRANGEBYRANK" => {
            if argv.len() != 4 {
                wrong_args(out, "zremrangebyrank");
            } else if let (Some(a), Some(b)) = (arg_i64(&argv[2]), arg_i64(&argv[3])) {
                emit_int(out, s.zremrangebyrank(&argv[1], a, b).map(|n| n as i64));
            } else {
                err(out, ERR_NOT_INT);
            }
        }
        b"ZREMRANGEBYSCORE" => cmd_zremrangebyscore(s, argv, out),
        b"ZSCAN" => cmd_zscan(s, argv, out),
        _ => return false,
    }
    true
}

/// Leading `ZADD` option tokens; returns `(flags, incr, first-score idx)`.
fn parse_zadd_flags(argv: &[Vec<u8>]) -> Result<(ZaddFlags, bool, usize), &'static str> {
    let mut f = ZaddFlags::default();
    let mut incr = false;
    let mut i = 2;
    while i < argv.len() {
        let a = &argv[i];
        if a.eq_ignore_ascii_case(b"NX") {
            f.nx = true;
        } else if a.eq_ignore_ascii_case(b"XX") {
            f.xx = true;
        } else if a.eq_ignore_ascii_case(b"GT") {
            f.gt = true;
        } else if a.eq_ignore_ascii_case(b"LT") {
            f.lt = true;
        } else if a.eq_ignore_ascii_case(b"CH") {
            f.ch = true;
        } else if a.eq_ignore_ascii_case(b"INCR") {
            incr = true;
        } else {
            break;
        }
        i += 1;
    }
    if !f.valid() {
        return Err("ERR GT, LT, and/or NX options at the same time are not compatible");
    }
    Ok((f, incr, i))
}

/// `ZADD key [NX|XX] [GT|LT] [CH] [INCR] score member [score member …]`.
fn cmd_zadd(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let (flags, incr, first) = match parse_zadd_flags(argv) {
        Ok(t) => t,
        Err(msg) => return err(out, msg),
    };
    if argv.len() < first + 2 || !(argv.len() - first).is_multiple_of(2) {
        return wrong_args(out, "zadd");
    }
    let mut pairs: Vec<(f64, &[u8])> = Vec::with_capacity((argv.len() - first) / 2);
    let mut i = first;
    while i < argv.len() {
        let Some(score) = arg_f64(&argv[i]) else {
            return err(out, ERR_NOT_FLOAT);
        };
        pairs.push((score, &argv[i + 1]));
        i += 2;
    }
    if incr {
        if pairs.len() != 1 {
            return err(out, "ERR INCR option supports a single increment-element pair");
        }
        return match s.zadd_incr(&argv[1], pairs[0].0, pairs[0].1, flags) {
            Ok(Some(next)) => bulk(out, &fmt_score(next)),
            Ok(None) => nil(out),
            Err(e) => kevy_err(out, &e),
        };
    }
    if flags == ZaddFlags::default() {
        return emit_int(out, s.zadd(&argv[1], &pairs).map(|n| n as i64));
    }
    match s.zadd_flags(&argv[1], &pairs, flags) {
        Ok(rep) => int(out, (if flags.ch { rep.changed } else { rep.added }) as i64),
        Err(e) => kevy_err(out, &e),
    }
}

/// `ZCOUNT key min max` — exclusive bounds route through the
/// bound-aware range read (the plain facade is inclusive-only).
fn cmd_zcount(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() != 4 {
        return wrong_args(out, "zcount");
    }
    let (Some(min), Some(max)) =
        (super::util::parse_score_bound(&argv[2]), super::util::parse_score_bound(&argv[3]))
    else {
        return err(out, ERR_MIN_MAX);
    };
    if !min.exclusive && !max.exclusive {
        return emit_int(out, s.zcount(&argv[1], min.value, max.value).map(|n| n as i64));
    }
    emit_int(out, s.zrange_by_score_excl(&argv[1], min, max).map(|v| v.len() as i64));
}

/// `ZRANGE`/`ZREVRANGE key start stop [WITHSCORES]` — by rank.
fn cmd_zrange(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>, rev: bool) {
    let name = if rev { "zrevrange" } else { "zrange" };
    if argv.len() < 4 || argv.len() > 5 {
        return wrong_args(out, name);
    }
    let withscores = argv.len() == 5;
    if withscores && !argv[4].eq_ignore_ascii_case(b"WITHSCORES") {
        return err(out, ERR_SYNTAX);
    }
    let (Some(a), Some(b)) = (arg_i64(&argv[2]), arg_i64(&argv[3])) else {
        return err(out, ERR_NOT_INT);
    };
    let res = if rev { s.zrevrange(&argv[1], a, b) } else { s.zrange(&argv[1], a, b) };
    match res {
        Ok(items) => emit_scored(out, &items, withscores),
        Err(e) => kevy_err(out, &e),
    }
}

/// The optional `[WITHSCORES] [LIMIT offset count]` tail (either
/// order, once each). `None` = the error is already encoded.
fn parse_range_modifiers(
    argv: &[Vec<u8>],
    out: &mut Vec<u8>,
) -> Option<(bool, Option<(i64, i64)>)> {
    let mut withscores = false;
    let mut limit: Option<(i64, i64)> = None;
    let mut i = 4;
    while i < argv.len() {
        let tok = &argv[i];
        if tok.eq_ignore_ascii_case(b"WITHSCORES") {
            if withscores {
                err(out, ERR_SYNTAX);
                return None;
            }
            withscores = true;
            i += 1;
        } else if tok.eq_ignore_ascii_case(b"LIMIT") {
            if limit.is_some() || i + 2 >= argv.len() {
                err(out, ERR_SYNTAX);
                return None;
            }
            let (Some(off), Some(cnt)) = (arg_i64(&argv[i + 1]), arg_i64(&argv[i + 2])) else {
                err(out, ERR_NOT_INT);
                return None;
            };
            limit = Some((off, cnt));
            i += 3;
        } else {
            err(out, ERR_SYNTAX);
            return None;
        }
    }
    Some((withscores, limit))
}

/// `ZRANGEBYSCORE key min max …` / `ZREVRANGEBYSCORE key max min …`
/// (bound order inverted for the reverse form, matching Redis).
fn cmd_zrangebyscore(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>, rev: bool) {
    let name = if rev { "zrevrangebyscore" } else { "zrangebyscore" };
    if argv.len() < 4 {
        return wrong_args(out, name);
    }
    let (lo_idx, hi_idx) = if rev { (3, 2) } else { (2, 3) };
    let (Some(min), Some(max)) = (
        super::util::parse_score_bound(&argv[lo_idx]),
        super::util::parse_score_bound(&argv[hi_idx]),
    ) else {
        return err(out, ERR_MIN_MAX);
    };
    let Some((withscores, limit)) = parse_range_modifiers(argv, out) else {
        return;
    };
    match s.zrange_by_score_excl(&argv[1], min, max) {
        Err(e) => kevy_err(out, &e),
        Ok(mut items) => {
            if rev {
                items.reverse();
            }
            if let Some((off, cnt)) = limit {
                apply_limit(&mut items, off, cnt);
            }
            emit_scored(out, &items, withscores);
        }
    }
}

/// Redis LIMIT semantics: negative count = all remaining.
fn apply_limit(items: &mut Vec<(Vec<u8>, f64)>, off: i64, cnt: i64) {
    let start = off.max(0) as usize;
    if start >= items.len() {
        items.clear();
    } else if cnt < 0 {
        items.drain(..start);
    } else {
        let end = (start + cnt as usize).min(items.len());
        *items = items[start..end].to_vec();
    }
}

/// `ZPOPMIN key [count]` — flat `[m, s, …]` reply.
fn cmd_zpopmin(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 2 || argv.len() > 3 {
        return wrong_args(out, "zpopmin");
    }
    let count = if argv.len() == 3 {
        let Some(c) = arg_i64(&argv[2]) else {
            return err(out, ERR_NOT_INT);
        };
        if c < 0 {
            return err(out, "ERR value is out of range, must be positive");
        }
        c as usize
    } else {
        1
    };
    match s.zpopmin(&argv[1], count) {
        Ok(items) => emit_scored(out, &items, true),
        Err(e) => kevy_err(out, &e),
    }
}

/// `ZPOPMIN.BELOW key below [count]` — the delayed-job primitive.
fn cmd_zpopmin_below(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 3 || argv.len() > 4 {
        return wrong_args(out, "zpopmin.below");
    }
    let Some(below) = arg_f64(&argv[2]) else {
        return err(out, ERR_NOT_FLOAT);
    };
    let count = if argv.len() == 4 {
        let Some(c) = arg_i64(&argv[3]) else {
            return err(out, ERR_NOT_INT);
        };
        if c < 0 {
            return err(out, "ERR value is out of range, must be positive");
        }
        c as usize
    } else {
        1
    };
    match s.zpopmin_below(&argv[1], below, count) {
        Ok(items) => emit_scored(out, &items, true),
        Err(e) => kevy_err(out, &e),
    }
}

/// `ZREMRANGEBYSCORE key min max` — exclusive bounds compose a
/// bound-aware read + `ZREM` of the hits (the plain facade is
/// inclusive-only); AOF carries the ZREM effect either way.
fn cmd_zremrangebyscore(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() != 4 {
        return wrong_args(out, "zremrangebyscore");
    }
    let (Some(min), Some(max)) =
        (super::util::parse_score_bound(&argv[2]), super::util::parse_score_bound(&argv[3]))
    else {
        return err(out, ERR_MIN_MAX);
    };
    if !min.exclusive && !max.exclusive {
        return emit_int(out, s.zremrangebyscore(&argv[1], min.value, max.value).map(|n| n as i64));
    }
    emit_int(out, zrem_range_excl(s, &argv[1], min, max));
}

fn zrem_range_excl(
    s: &Store,
    key: &[u8],
    min: ScoreBound,
    max: ScoreBound,
) -> crate::KevyResult<i64> {
    let hits = s.zrange_by_score_excl(key, min, max)?;
    if hits.is_empty() {
        return Ok(0);
    }
    let members: Vec<&[u8]> = hits.iter().map(|(m, _)| m.as_slice()).collect();
    Ok(s.zrem(key, &members)? as i64)
}

/// `ZSCAN key cursor [MATCH pattern] [COUNT n]` — one full batch,
/// cursor "0", `(member, score)` interleaved.
fn cmd_zscan(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 3 {
        return wrong_args(out, "zscan");
    }
    if arg_i64(&argv[2]).is_none() {
        return err(out, ERR_NOT_INT);
    }
    let Some(pat) = super::parse_match_count(argv, 3) else {
        return err(out, ERR_SYNTAX);
    };
    match s.zrange(&argv[1], 0, -1) {
        Err(e) => kevy_err(out, &e),
        Ok(items) => {
            let mut flat: Vec<Vec<u8>> = Vec::with_capacity(items.len() * 2);
            for (m, sc) in items {
                if pat.as_ref().is_none_or(|p| kevy_store::glob_match(p, &m)) {
                    flat.push(m);
                    flat.push(fmt_score(sc));
                }
            }
            super::emit_scan_page(out, b"0", &flat);
        }
    }
}
