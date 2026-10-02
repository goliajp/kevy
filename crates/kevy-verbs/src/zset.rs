//! Sorted-set commands on one key: `ZADD` with its Redis 6.2 flags,
//! point reads, counts and removals. Range reads and pops live in
//! `zset_range`; the multi-key algebra is not here.

use kevy_resp::{ArgvView, CmdError, encode_error, encode_null_bulk};
use kevy_store::{ScoreCompare, SetCondition, Store, ZaddFlags};

use crate::args::{arg_f64, arg_i64, parse_score_bound, upper_verb, with_rest};
use crate::reply::{ERR_NOT_FLOAT, ERR_NOT_INT, emit_int_result, store_err, wrong_args};
use crate::{Effect, changed, zset_range};

const ERR_MIN_MAX: &str = "ERR min or max is not a float";

/// One sorted-set command; `None` = the verb is not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per zset verb.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        b"ZADD" => {
            zadd(store, args, out);
            Effect::Write
        }
        b"ZSCORE" => {
            if args.len() == 3 {
                match store.zscore(&args[1], &args[2]) {
                    Ok(Some(sc)) => kevy_resp::encode_bulk_double(out, sc),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                wrong_args(out, "zscore");
            }
            Effect::Read
        }
        b"ZCARD" => {
            if args.len() == 2 {
                emit_int_result(store.zcard(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "zcard");
            }
            Effect::Read
        }
        b"ZREM" => {
            if args.len() < 3 {
                wrong_args(out, "zrem");
                return Some(Effect::Unchanged);
            }
            let res = with_rest(args, 2, |rest| store.zrem(&args[1], rest));
            removed(res, out)
        }
        b"ZINCRBY" => {
            if args.len() != 4 {
                wrong_args(out, "zincrby");
            } else if let Some(incr) = arg_f64(&args[2]) {
                match store.zincrby(&args[1], incr, &args[3]) {
                    Ok(sc) => kevy_resp::encode_bulk_double(out, sc),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_FLOAT);
            }
            Effect::Write
        }
        b"ZCOUNT" => {
            if args.len() != 4 {
                wrong_args(out, "zcount");
            } else if let (Some(min), Some(max)) =
                (parse_score_bound(&args[2]), parse_score_bound(&args[3]))
            {
                emit_int_result(store.zcount(&args[1], min, max).map(|n| n as i64), out);
            } else {
                encode_error(out, ERR_MIN_MAX);
            }
            Effect::Read
        }
        b"ZREMRANGEBYRANK" => {
            if args.len() != 4 {
                wrong_args(out, "zremrangebyrank");
                return Some(Effect::Unchanged);
            }
            let (Some(s), Some(e)) = (arg_i64(&args[2]), arg_i64(&args[3])) else {
                encode_error(out, ERR_NOT_INT);
                return Some(Effect::Unchanged);
            };
            removed(store.zrem_range_by_rank(&args[1], s, e), out)
        }
        b"ZREMRANGEBYSCORE" => {
            if args.len() != 4 {
                wrong_args(out, "zremrangebyscore");
                return Some(Effect::Unchanged);
            }
            let (Some(min), Some(max)) = (parse_score_bound(&args[2]), parse_score_bound(&args[3]))
            else {
                encode_error(out, ERR_MIN_MAX);
                return Some(Effect::Unchanged);
            };
            removed(store.zrem_range_by_score(&args[1], min, max), out)
        }
        _ => return zset_range::exec(cmd, store, args, out),
    })
}

/// A removal count as the reply; nothing removed is no change.
fn removed(res: Result<usize, kevy_store::StoreError>, out: &mut Vec<u8>) -> Effect {
    let any = matches!(res, Ok(n) if n > 0);
    emit_int_result(res.map(|n| n as i64), out);
    changed(any)
}

/// The leading `ZADD` option tokens (`NX` / `XX` / `GT` / `LT` / `CH` /
/// `INCR`): the flags, whether `INCR` was given, and the index of the
/// first score.
///
/// ```
/// let argv = kevy_resp::Argv::from(vec![b"ZADD".to_vec(), b"z".to_vec(), b"NX".to_vec(), b"1".to_vec(), b"m".to_vec()]);
/// let (flags, incr, first) = kevy_verbs::cmd::parse_zadd_flags(&argv).unwrap();
/// assert!(flags.condition() == kevy_store::SetCondition::IfAbsent && !incr && first == 3);
/// ```
pub fn parse_zadd_flags<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<(ZaddFlags, bool, usize), CmdError> {
    const CLASH: CmdError =
        CmdError::Wire("ERR GT, LT, and/or NX options at the same time are not compatible");
    let (mut nx, mut xx, mut gt, mut lt, mut ch, mut incr) =
        (false, false, false, false, false, false);
    let mut i = 2;
    let mut buf = [0u8; 32];
    while i < args.len() {
        // every option is a word; a score starts with a letter only as an
        // `inf` spelling, which the match below turns away
        if !args[i].first().is_some_and(u8::is_ascii_alphabetic) {
            break;
        }
        let flag = match upper_verb(&args[i], &mut buf) {
            b"NX" => &mut nx,
            b"XX" => &mut xx,
            b"GT" => &mut gt,
            b"LT" => &mut lt,
            b"CH" => &mut ch,
            b"INCR" => &mut incr,
            _ => break,
        };
        *flag = true;
        i += 1;
    }
    let condition = match (nx, xx) {
        (false, false) => SetCondition::Always,
        (true, false) => SetCondition::IfAbsent,
        (false, true) => SetCondition::IfPresent,
        (true, true) => return Err(CLASH),
    };
    let compare = match (gt, lt) {
        (false, false) => ScoreCompare::Any,
        (true, false) => ScoreCompare::Greater,
        (false, true) => ScoreCompare::Less,
        (true, true) => return Err(CLASH),
    };
    let f = ZaddFlags::new(condition, compare).ok_or(CLASH)?.with_ch(ch);
    Ok((f, incr, i))
}

/// `ZADD key [NX|XX] [GT|LT] [CH] [INCR] score member [score member ...]`.
/// Without flags it takes the store's plain `zadd` path.
fn zadd<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    let (flags, incr, first) = match parse_zadd_flags(args) {
        Ok(t) => t,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    if args.len() < first + 2 || !(args.len() - first).is_multiple_of(2) {
        return wrong_args(out, "zadd");
    }
    // one pair is the common call; it stays on the stack
    if args.len() - first == 2 {
        let Some(score) = arg_f64(&args[first]) else {
            return encode_error(out, ERR_NOT_FLOAT);
        };
        return zadd_pairs(store, args, &[(score, &args[first + 1])], flags, incr, out);
    }
    let mut pairs: Vec<(f64, &[u8])> = Vec::with_capacity((args.len() - first) / 2);
    let mut i = first;
    while i < args.len() {
        let Some(score) = arg_f64(&args[i]) else {
            return encode_error(out, ERR_NOT_FLOAT);
        };
        pairs.push((score, &args[i + 1]));
        i += 2;
    }
    zadd_pairs(store, args, &pairs, flags, incr, out);
}

fn zadd_pairs<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    pairs: &[(f64, &[u8])],
    flags: ZaddFlags,
    incr: bool,
    out: &mut Vec<u8>,
) {
    if incr {
        if pairs.len() != 1 {
            return encode_error(out, "ERR INCR option supports a single increment-element pair");
        }
        return match store.zadd_incr(&args[1], pairs[0].0, pairs[0].1, flags) {
            Ok(Some(next)) => kevy_resp::encode_bulk_double(out, next),
            Ok(None) => encode_null_bulk(out),
            Err(e) => store_err(out, e),
        };
    }
    if flags == ZaddFlags::default() {
        return emit_int_result(store.zadd(&args[1], pairs).map(|n| n as i64), out);
    }
    match store.zadd_flags(&args[1], pairs, flags) {
        Ok(rep) => {
            let n = if flags.ch() { rep.changed } else { rep.added };
            emit_int_result(Ok(n as i64), out);
        }
        Err(e) => store_err(out, e),
    }
}
