//! v1.27.3 BullMQ-enabling helpers — `LPOS`, `ZPOPMIN`,
//! `ZREVRANGEBYSCORE`. Their argv parsers are too verbose to inline in
//! the per-type `match` tables in [`crate::dispatch_collections`], so
//! the table rows delegate to the `cmd_*` functions defined here.

use crate::cmd::{arg_i64, emit_zrange, fmt_score, parse_score_bound, store_err, wrong_args, ERR_NOT_INT};
use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error, encode_integer,
    encode_null_bulk,
};
use kevy_store::Store;

/// `LPOS key element [RANK n] [COUNT n] [MAXLEN n]` — see Redis docs.
///
/// `RANK 1` (default) = first match from the head; `RANK -1` = first
/// match from the tail (return absolute index). `COUNT 0` returns all
/// matches; `COUNT n` caps to `n`; `COUNT` absent returns a single
/// match as integer (or nil bulk if none). `MAXLEN 0` is unlimited;
/// otherwise stops the scan after that many elements.
pub(crate) fn cmd_lpos<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "lpos");
    }
    let mut rank: i64 = 1;
    let mut count: Option<i64> = None;
    let mut maxlen: usize = 0;
    let mut i = 3;
    while i < args.len() {
        let tok = &args[i];
        if tok.eq_ignore_ascii_case(b"RANK") {
            if i + 1 >= args.len() {
                return encode_error(out, "ERR syntax error");
            }
            let Some(r) = arg_i64(&args[i + 1]) else {
                return encode_error(out, ERR_NOT_INT);
            };
            if r == 0 {
                return encode_error(
                    out,
                    "ERR RANK can't be zero: use 1 to start from the first match going forward, or -1 from the last match going backward.",
                );
            }
            rank = r;
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"COUNT") {
            if i + 1 >= args.len() {
                return encode_error(out, "ERR syntax error");
            }
            let Some(c) = arg_i64(&args[i + 1]) else {
                return encode_error(out, ERR_NOT_INT);
            };
            if c < 0 {
                return encode_error(out, "ERR COUNT can't be negative");
            }
            count = Some(c);
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"MAXLEN") {
            if i + 1 >= args.len() {
                return encode_error(out, "ERR syntax error");
            }
            let Some(m) = arg_i64(&args[i + 1]) else {
                return encode_error(out, ERR_NOT_INT);
            };
            if m < 0 {
                return encode_error(out, "ERR MAXLEN can't be negative");
            }
            maxlen = m as usize;
            i += 2;
        } else {
            return encode_error(out, "ERR syntax error");
        }
    }
    match store.lpos(&args[1], &args[2], rank, count, maxlen) {
        Err(e) => store_err(out, e),
        Ok(hits) => match count {
            None => {
                if let Some(idx) = hits.first() {
                    encode_integer(out, *idx);
                } else {
                    encode_null_bulk(out);
                }
            }
            Some(_) => {
                encode_array_len(out, hits.len() as i64);
                for idx in &hits {
                    encode_integer(out, *idx);
                }
            }
        },
    }
}

/// `BZPOPMIN key [key ...] timeout` — blocking `ZPOPMIN` across a set of
/// candidate sorted sets. On hit, replies with a 3-bulk array:
/// `*3 [<key>, <member>, <score>]` (RESP2). On empty + timeout=0 the
/// dispatcher parks the conn forever; otherwise the reactor's
/// blocked-timeout tick fires a nil array reply (`*-1\r\n`) at the
/// deadline.
///
/// Behavior split mirrors `cmd_blpop`:
/// - Multi-key form (`len > 3`) — leaves `out` untouched so the
///   dispatcher parks the conn across all watched keys via the
///   cross-shard arbiter; each per-key wake replays the single-key form
///   built by `cmd_block_serve::pop_serve(b"BZPOPMIN", key)`.
/// - Single-key form (`len == 3`) — pops one member with the lowest
///   score; on empty, leaves `out` untouched so the in-shard fast path
///   registers the conn as a waiter on `args[1]`.
pub(crate) fn cmd_bzpopmin<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() < 3 {
        return wrong_args(out, "bzpopmin");
    }
    let timeout_idx = args.len() - 1;
    let valid = std::str::from_utf8(&args[timeout_idx])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .is_some_and(|f| f.is_finite() && f >= 0.0);
    if !valid {
        return encode_error(out, "ERR timeout is not a float or out of range");
    }
    if args.len() > 3 {
        // Multi-key: leave out untouched → arbiter parks + per-key
        // replay built from `BZPOPMIN key 0` (the len == 3 path here).
        return;
    }
    match store.zpopmin(&args[1], 1) {
        Err(e) => store_err(out, e),
        Ok(items) => {
            if let Some((member, score)) = items.into_iter().next() {
                encode_array_len(out, 3);
                encode_bulk(out, &args[1]);
                encode_bulk(out, &member);
                encode_bulk(out, &fmt_score(score));
            }
            // else: empty key — out untouched; runtime parks the conn.
        }
    }
}

/// `ZPOPMIN key [count]` — pop the `count` lowest-scored members and
/// reply with `[m1, s1, m2, s2, ...]` (RESP2 V2 flat shape, mirrors
/// `ZRANGE ... WITHSCORES`). `count` defaults to `1`.
pub(crate) fn cmd_zpopmin<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 2 || args.len() > 3 {
        return wrong_args(out, "zpopmin");
    }
    let count = if args.len() == 3 {
        let Some(c) = arg_i64(&args[2]) else {
            return encode_error(out, ERR_NOT_INT);
        };
        if c < 0 {
            return encode_error(out, "ERR value is out of range, must be positive");
        }
        c as usize
    } else {
        1
    };
    match store.zpopmin(&args[1], count) {
        Err(e) => store_err(out, e),
        Ok(items) => {
            encode_array_len(out, (items.len() * 2) as i64);
            for (m, sc) in &items {
                encode_bulk(out, m);
                encode_bulk(out, &fmt_score(*sc));
            }
        }
    }
}

/// `ZREVRANGEBYSCORE key max min [WITHSCORES] [LIMIT offset count]`.
/// Note the inverted bound order vs `ZRANGEBYSCORE`: max first, min
/// second.
pub(crate) fn cmd_zrevrangebyscore<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 4 {
        return wrong_args(out, "zrevrangebyscore");
    }
    // argv[2] is MAX, argv[3] is MIN — flip to the (min, max) order
    // the backend uses.
    let (Some(max), Some(min)) = (parse_score_bound(&args[2]), parse_score_bound(&args[3])) else {
        return encode_error(out, "ERR min or max is not a float");
    };
    let mut withscores = false;
    let mut limit: Option<(i64, i64)> = None;
    let mut i = 4;
    while i < args.len() {
        let tok = &args[i];
        if tok.eq_ignore_ascii_case(b"WITHSCORES") {
            if withscores {
                return encode_error(out, "ERR syntax error");
            }
            withscores = true;
            i += 1;
        } else if tok.eq_ignore_ascii_case(b"LIMIT") {
            if limit.is_some() || i + 2 >= args.len() {
                return encode_error(out, "ERR syntax error");
            }
            let Some(off) = arg_i64(&args[i + 1]) else {
                return encode_error(out, ERR_NOT_INT);
            };
            let Some(cnt) = arg_i64(&args[i + 2]) else {
                return encode_error(out, ERR_NOT_INT);
            };
            limit = Some((off, cnt));
            i += 3;
        } else {
            return encode_error(out, "ERR syntax error");
        }
    }
    let res = store.zrev_range_by_score(&args[1], min, max);
    match res {
        Err(e) => store_err(out, e),
        Ok(mut items) => {
            if let Some((off, cnt)) = limit {
                let start = off.max(0) as usize;
                if start >= items.len() {
                    items.clear();
                } else if cnt < 0 {
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

// ─────────────────────────────────────────────────────────────────────
// v1.27.5 ecosystem-unblock additions: SSCAN / HSCAN / ZSCAN
// ─────────────────────────────────────────────────────────────────────
//
// Cursor-based iterators for Set / Hash / Sorted-Set. Sidekiq's
// scheduler thread depends on SSCAN ("processes" set). kevy returns
// every element in one batch (cursor = "0") — matches Redis's
// small-collection optimisation where SCAN doesn't actually paginate.
// COUNT is parsed but ignored (we always return everything).
//
// Reply shape:
//   *2\r\n
//   $1\r\n0\r\n         ← next-cursor as bulk string
//   *N\r\n              ← elements array
//   $<len>\r\n<bytes>\r\n  (repeated)

/// Parse `[MATCH pattern] [COUNT n]` modifiers starting at argv idx.
/// Returns `(maybe_pattern, _count_ignored)` or None on syntax error.
fn parse_scan_opts<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
) -> Option<Option<Vec<u8>>> {
    let mut pat: Option<Vec<u8>> = None;
    let mut i = start;
    while i < args.len() {
        let tok = &args[i];
        if tok.eq_ignore_ascii_case(b"MATCH") {
            if i + 1 >= args.len() { return None; }
            pat = Some(args[i + 1].to_vec());
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"COUNT") {
            if i + 1 >= args.len() { return None; }
            // Validate but ignore — kevy returns everything in one shot.
            arg_i64(&args[i + 1])?;
            i += 2;
        } else {
            return None; // unknown modifier
        }
    }
    Some(pat)
}

fn emit_scan_reply(out: &mut Vec<u8>, elems: &[Vec<u8>]) {
    encode_array_len(out, 2);
    encode_bulk(out, b"0"); // cursor = "0" (done in one batch)
    encode_array_len(out, elems.len() as i64);
    for e in elems {
        encode_bulk(out, e);
    }
}

/// `SSCAN key cursor [MATCH pattern] [COUNT n]`
pub(crate) fn cmd_sscan<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "sscan");
    }
    if arg_i64(&args[2]).is_none() {
        return encode_error(out, ERR_NOT_INT);
    }
    let Some(pat) = parse_scan_opts(args, 3) else {
        return encode_error(out, "ERR syntax error");
    };
    match store.smembers(&args[1]) {
        Err(e) => store_err(out, e),
        Ok(all) => {
            let filtered: Vec<Vec<u8>> = match pat {
                None => all,
                Some(p) => all.into_iter()
                    .filter(|m| kevy_store::glob_match(&p, m))
                    .collect(),
            };
            emit_scan_reply(out, &filtered);
        }
    }
}

/// `HSCAN key cursor [MATCH pattern] [COUNT n]` — field-then-value
/// pairs interleaved.
pub(crate) fn cmd_hscan<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "hscan");
    }
    if arg_i64(&args[2]).is_none() {
        return encode_error(out, ERR_NOT_INT);
    }
    let Some(pat) = parse_scan_opts(args, 3) else {
        return encode_error(out, "ERR syntax error");
    };
    match store.hgetall(&args[1]) {
        Err(e) => store_err(out, e),
        Ok(flat) => {
            // hgetall returns [field, value, field, value, ...] flat.
            // Filter pairs by MATCH on the field name.
            let mut out_v: Vec<Vec<u8>> = Vec::with_capacity(flat.len());
            for pair in flat.chunks(2) {
                if pair.len() != 2 { continue; }
                let field = &pair[0];
                let val = &pair[1];
                if pat.as_ref().is_none_or(|p| kevy_store::glob_match(p, field)) {
                    out_v.push(field.clone());
                    out_v.push(val.clone());
                }
            }
            emit_scan_reply(out, &out_v);
        }
    }
}

/// `ZSCAN key cursor [MATCH pattern] [COUNT n]` — member-then-score
/// pairs interleaved (score as fmt_score-formatted bulk).
pub(crate) fn cmd_zscan<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "zscan");
    }
    if arg_i64(&args[2]).is_none() {
        return encode_error(out, ERR_NOT_INT);
    }
    let Some(pat) = parse_scan_opts(args, 3) else {
        return encode_error(out, "ERR syntax error");
    };
    match store.zrange(&args[1], 0, -1) {
        Err(e) => store_err(out, e),
        Ok(items) => {
            let mut out_v: Vec<Vec<u8>> = Vec::with_capacity(items.len() * 2);
            for (m, sc) in items {
                if pat.as_ref().is_none_or(|p| kevy_store::glob_match(p, &m)) {
                    out_v.push(m);
                    out_v.push(fmt_score(sc));
                }
            }
            emit_scan_reply(out, &out_v);
        }
    }
}
