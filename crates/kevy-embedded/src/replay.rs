//! Apply a single AOF command — the inverse of
//! `kevy_persist::Aof::rewrite_from` and the existing `Aof::append`
//! shape from the server side.
//!
//! Kept private to `kevy-embedded`: the server (kevy crate) replays via
//! `KevyCommands::dispatch`, which speaks the same canonical verbs but
//! pulls in the whole network/dispatch stack. This module is a small,
//! focused subset — only the mutating verbs we ever write into the AOF —
//! so the embedded crate stays free of `kevy-rt` / `kevy-sys` deps.

use kevy_persist::Argv;
use kevy_store::{ScoreBound, Store};
use std::time::Duration;

/// Apply one command from an AOF frame to `store`. Mirrors the verbs
/// `Aof::rewrite_from` + the server's write-path AOF logging emit:
/// SET / HSET / RPUSH / SADD / ZADD / PEXPIRE — plus the
/// "append-side" verbs (DEL, FLUSHALL, EXPIRE, PERSIST, INCR family,
/// LPUSH, LPOP, RPOP, HDEL, SREM, ZREM, LSET, LREM, LTRIM, SPOP) that
/// the server's `KevyCommands::is_write` set logs as-is.
///
/// Invariant (v1.15.1): every verb any `kevy-embedded` facade method
/// passes to `commit_write` MUST have an arm here — a missing arm is
/// silent data loss on reopen (`store_tests_replay_all.rs` is the
/// guard). The v2.1 OP_TABLE makes this cross-check structural.
// fn-length exemption: pure data-driven verb match table — one flat
// arm per replayed verb, no control flow beyond per-arm arg plumbing.
// LOC-WAIVER: data-driven AOF verb replay table — one store-call arm per verb.
pub(crate) fn apply(store: &mut Store, args: &Argv) {
    let Some(name) = args.first() else { return };
    let verb = ascii_upper(name);
    match verb.as_slice() {
        b"SET" => apply_set(store, args),
        b"DEL" => {
            let keys: Vec<Vec<u8>> = args.iter().skip(1).map(<[u8]>::to_vec).collect();
            store.del(&keys);
        }
        b"INCR" => {
            if let Some(k) = args.get(1) {
                let _ = store.incr_by(k, 1);
            }
        }
        b"DECR" => {
            if let Some(k) = args.get(1) {
                let _ = store.incr_by(k, -1);
            }
        }
        b"INCRBY" => apply_incr_by(store, args, false),
        b"DECRBY" => apply_incr_by(store, args, true),
        b"INCRBYFLOAT" => {
            if let (Some(k), Some(amt)) = (args.get(1), args.get(2))
                && let Some(d) = parse_f64(amt)
            {
                let _ = store.incr_by_float(k, d);
            }
        }
        b"APPEND" => {
            if let (Some(k), Some(v)) = (args.get(1), args.get(2)) {
                let _ = store.append(k, v);
            }
        }
        b"SETBIT" => {
            if let (Some(k), Some(off), Some(v)) = (args.get(1), args.get(2), args.get(3))
                && let (Some(offset), Some(bit)) = (parse_u64(off), parse_u64(v))
            {
                let _ = store.setbit(k, offset, (bit != 0) as u8);
            }
        }
        b"SETRANGE" => {
            if let (Some(k), Some(off), Some(v)) = (args.get(1), args.get(2), args.get(3))
                && let Some(offset) = parse_u64(off)
            {
                let _ = store.setrange(k, offset, v);
            }
        }
        b"GETSET" => {
            if let (Some(k), Some(v)) = (args.get(1), args.get(2)) {
                let _ = store.getset(k, v.to_vec());
            }
        }
        b"GETDEL" => {
            if let Some(k) = args.get(1) {
                let _ = store.getdel(k);
            }
        }
        b"EXPIRE" => apply_expire(store, args, 1_000),
        b"PEXPIRE" => apply_expire(store, args, 1),
        b"EXPIREAT" => apply_expireat(store, args, 1_000),
        b"PEXPIREAT" => apply_expireat(store, args, 1),
        b"PERSIST" => {
            if let Some(k) = args.get(1) {
                store.persist(k);
            }
        }
        b"FLUSHDB" | b"FLUSHALL" => store.flushall(),
        b"HSET" => apply_hset(store, args),
        // v2.4 hash field TTLs: HPEXPIREAT is the canonical logged
        // form; HPERSIST logs itself.
        b"HPEXPIREAT" => apply_hpexpireat(store, args),
        b"HPERSIST" => apply_hpersist(store, args),
        b"HDEL" => apply_pairs_strip(store, args, |s, k, fs| {
            let _ = s.hdel(k, fs);
        }),
        b"HINCRBY" => {
            if let (Some(k), Some(f), Some(amt)) = (args.get(1), args.get(2), args.get(3))
                && let Some(d) = parse_i64(amt)
            {
                let _ = store.hincrby(k, f, d);
            }
        }
        b"HINCRBYFLOAT" => {
            if let (Some(k), Some(f), Some(amt)) = (args.get(1), args.get(2), args.get(3))
                && let Some(d) = parse_f64(amt)
            {
                let _ = store.hincrbyfloat(k, f, d);
            }
        }
        b"HSETNX" => {
            if let (Some(k), Some(f), Some(v)) = (args.get(1), args.get(2), args.get(3)) {
                let _ = store.hsetnx(k, f, v);
            }
        }
        b"RPUSH" => apply_pairs_strip(store, args, |s, k, vs| {
            let _ = s.rpush(k, vs);
        }),
        b"LPUSH" => apply_pairs_strip(store, args, |s, k, vs| {
            let _ = s.lpush(k, vs);
        }),
        b"LPOP" => apply_pop(store, args, false),
        b"RPOP" => apply_pop(store, args, true),
        b"LSET" => {
            if let (Some(k), Some(i), Some(v)) = (args.get(1), args.get(2), args.get(3))
                && let Some(idx) = parse_i64(i)
            {
                let _ = store.lset(k, idx, v);
            }
        }
        b"LREM" => {
            if let (Some(k), Some(c), Some(v)) = (args.get(1), args.get(2), args.get(3))
                && let Some(count) = parse_i64(c)
            {
                let _ = store.lrem(k, count, v);
            }
        }
        b"LTRIM" => {
            if let (Some(k), Some(s), Some(e)) = (args.get(1), args.get(2), args.get(3))
                && let (Some(start), Some(stop)) = (parse_i64(s), parse_i64(e))
            {
                let _ = store.ltrim(k, start, stop);
            }
        }
        b"LINSERT" => {
            if let (Some(k), Some(dir), Some(pivot), Some(v)) =
                (args.get(1), args.get(2), args.get(3), args.get(4))
            {
                let before = dir.eq_ignore_ascii_case(b"BEFORE");
                let _ = store.linsert(k, before, pivot, v);
            }
        }
        b"RENAME" => {
            if let (Some(src), Some(dst)) = (args.get(1), args.get(2)) {
                let _ = store.rename(src, dst, false);
            }
        }
        b"RENAMENX" => {
            if let (Some(src), Some(dst)) = (args.get(1), args.get(2)) {
                let _ = store.rename(src, dst, true);
            }
        }
        b"SADD" => apply_pairs_strip(store, args, |s, k, ms| {
            let _ = s.sadd(k, ms);
        }),
        b"SREM" => apply_pairs_strip(store, args, |s, k, ms| {
            let _ = s.srem(k, ms);
        }),
        b"SPOP" => {
            if let Some(k) = args.get(1) {
                let count = args
                    .get(2)
                    .and_then(parse_i64)
                    .map_or(1usize, |c| c.max(0) as usize);
                let _ = store.spop(k, count);
            }
        }
        b"ZADD" => apply_zadd(store, args),
        b"ZREM" => apply_pairs_strip(store, args, |s, k, ms| {
            let _ = s.zrem(k, ms);
        }),
        b"ZINCRBY" => {
            if let (Some(k), Some(incr), Some(m)) = (args.get(1), args.get(2), args.get(3))
                && let Some(d) = parse_f64(incr)
            {
                let _ = store.zincrby(k, d, m);
            }
        }
        // ZPOPMIN replays deterministically: min-by-(score, member) is
        // well-defined, so re-popping `count` reproduces the live pop.
        b"ZPOPMIN" => {
            if let Some(k) = args.get(1) {
                let count = args
                    .get(2)
                    .and_then(parse_i64)
                    .map_or(1usize, |c| c.max(0) as usize);
                let _ = store.zpopmin(k, count);
            }
        }
        b"ZREMRANGEBYRANK" => {
            if let (Some(k), Some(s), Some(e)) = (args.get(1), args.get(2), args.get(3))
                && let (Some(start), Some(stop)) = (parse_i64(s), parse_i64(e))
            {
                let _ = store.zrem_range_by_rank(k, start, stop);
            }
        }
        b"ZREMRANGEBYSCORE" => {
            if let (Some(k), Some(mn), Some(mx)) = (args.get(1), args.get(2), args.get(3))
                && let (Some(min), Some(max)) = (parse_f64(mn), parse_f64(mx))
            {
                let _ = store.zrem_range_by_score(
                    k,
                    ScoreBound { value: min, exclusive: false },
                    ScoreBound { value: max, exclusive: false },
                );
            }
        }
        _ => {
            // Unknown verb in the AOF: silently skip. Forward-compat with
            // logs written by a newer kevy. Snapshot+verb-tag bumps would
            // catch corruption.
        }
    }
}

fn apply_set(store: &mut Store, args: &Argv) {
    if let (Some(k), Some(v)) = (args.get(1), args.get(2)) {
        // v1.0 AOF dump emits plain SET key value (no NX/EX/PX trailing);
        // server append also logs the raw arg list. Either way, the keyspace
        // semantics are "overwrite with no TTL" — which is what we replay.
        store.set(k, v.to_vec(), None, false, false);
    }
}

fn apply_incr_by(store: &mut Store, args: &Argv, negate: bool) {
    if let (Some(k), Some(amt)) = (args.get(1), args.get(2))
        && let Some(d) = parse_i64(amt)
    {
        let _ = store.incr_by(k, if negate { -d } else { d });
    }
}

fn apply_expire(store: &mut Store, args: &Argv, unit_ms: u64) {
    if let (Some(k), Some(t)) = (args.get(1), args.get(2))
        && let Some(n) = parse_u64(t)
    {
        store.expire(k, Duration::from_millis(n.saturating_mul(unit_ms)));
    }
}

/// `EXPIREAT`/`PEXPIREAT` replay: the argument is an **absolute** Unix
/// timestamp, so the deadline is reconstructed unchanged regardless of when
/// the replay runs (a past deadline drops the key). This is the persistence-
/// stable form the write path now emits in place of relative `PEXPIRE`.
fn apply_expireat(store: &mut Store, args: &Argv, unit_ms: u64) {
    if let (Some(k), Some(t)) = (args.get(1), args.get(2))
        && let Some(n) = parse_u64(t)
    {
        store.expire_at_unix_ms(k, n.saturating_mul(unit_ms));
    }
}


/// `HPEXPIREAT key unix-ms FIELDS n f…` (v2.4 canonical field-TTL frame).
fn apply_hpexpireat(store: &mut kevy_store::Store, args: &Argv) {
    if args.len() < 6 {
        return;
    }
    let Ok(deadline) = std::str::from_utf8(&args[2]).unwrap_or("").parse::<u64>() else {
        return;
    };
    let fields: Vec<&[u8]> = (5..args.len()).map(|i| &args[i] as &[u8]).collect();
    let _ = store.hexpire_at(&args[1], &fields, deadline, kevy_store::HExpireCond::Always);
}

/// `HPERSIST key FIELDS n f…`.
fn apply_hpersist(store: &mut kevy_store::Store, args: &Argv) {
    if args.len() < 5 {
        return;
    }
    let fields: Vec<&[u8]> = (4..args.len()).map(|i| &args[i] as &[u8]).collect();
    let _ = store.hpersist(&args[1], &fields);
}

fn apply_hset(store: &mut Store, args: &Argv) {
    let Some(k) = args.get(1) else { return };
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut i = 2;
    while i + 1 < args.len() {
        pairs.push((args[i].to_vec(), args[i + 1].to_vec()));
        i += 2;
    }
    if !pairs.is_empty() {
        let _ = store.hset(k, &pairs);
    }
}

fn apply_pop(store: &mut Store, args: &Argv, from_tail: bool) {
    if let Some(k) = args.get(1) {
        let count = args
            .get(2)
            .and_then(parse_i64)
            .map_or(1usize, |c| c.max(0) as usize);
        let _ = if from_tail {
            store.rpop(k, count)
        } else {
            store.lpop(k, count)
        };
    }
}

/// Parse the leading Redis 6.2 ZADD flag tokens; returns the flags,
/// the INCR marker, and the offset of the first score token.
fn parse_zadd_flags(args: &Argv) -> (kevy_store::ZaddFlags, bool, usize) {
    let mut flags = kevy_store::ZaddFlags::default();
    let mut incr = false;
    let mut i = 2;
    while i < args.len() {
        let a = &args[i];
        if a.eq_ignore_ascii_case(b"NX") {
            flags.nx = true;
        } else if a.eq_ignore_ascii_case(b"XX") {
            flags.xx = true;
        } else if a.eq_ignore_ascii_case(b"GT") {
            flags.gt = true;
        } else if a.eq_ignore_ascii_case(b"LT") {
            flags.lt = true;
        } else if a.eq_ignore_ascii_case(b"CH") {
            // CH only shapes the reply — no replay effect.
        } else if a.eq_ignore_ascii_case(b"INCR") {
            incr = true;
        } else {
            break;
        }
        i += 1;
    }
    (flags, incr, i)
}

fn apply_zadd(store: &mut Store, args: &Argv) {
    let Some(k) = args.get(1) else { return };
    // v2.1: a primary's frame stream may carry the Redis 6.2 flag
    // tokens (`ZADD key [NX|XX] [GT|LT] [CH] score member …`). Apply
    // them — misparsing a flag as a score would shift every following
    // pair. (Embedded's own AOF never contains flags: the facades log
    // the applied effect as plain ZADD.)
    let (flags, incr, mut i) = parse_zadd_flags(args);
    let mut pairs: Vec<(f64, Vec<u8>)> = Vec::new();
    while i + 1 < args.len() {
        if let Some(score) = parse_f64(&args[i]) {
            pairs.push((score, args[i + 1].to_vec()));
        }
        i += 2;
    }
    if pairs.is_empty() || !flags.valid() {
        return;
    }
    if incr {
        // `ZADD … INCR delta member` is an increment, NOT an absolute
        // score — applying it as one would silently diverge.
        if pairs.len() == 1 {
            let _ = store.zadd_incr(k, pairs[0].0, &pairs[0].1, flags);
        }
    } else if flags == kevy_store::ZaddFlags::default() {
        let _ = store.zadd(k, &pairs);
    } else {
        let borrowed: Vec<(f64, &[u8])> =
            pairs.iter().map(|(s, m)| (*s, m.as_slice())).collect();
        let _ = store.zadd_flags_borrowed(k, &borrowed, flags);
    }
}

/// Common shape: `VERB key item1 item2 ...` → call `f(store, key, &items)`.
fn apply_pairs_strip<F>(store: &mut Store, args: &Argv, f: F)
where
    F: FnOnce(&mut Store, &[u8], &[Vec<u8>]),
{
    let Some(k) = args.get(1) else { return };
    let rest: Vec<Vec<u8>> = args.iter().skip(2).map(<[u8]>::to_vec).collect();
    if !rest.is_empty() {
        f(store, k, &rest);
    }
}

fn ascii_upper(b: &[u8]) -> Vec<u8> {
    b.iter().map(u8::to_ascii_uppercase).collect()
}

fn parse_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

fn parse_u64(b: &[u8]) -> Option<u64> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

fn parse_f64(b: &[u8]) -> Option<f64> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;

/// Parity manifest (v2.1): every verb `apply` has an arm for. The
/// v2.0.21 invariant lives in the ops_table cross-check: any verb an
/// embedded facade logs MUST appear here.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const REPLAY_VERBS: &[&str] = &[
    "SET", "DEL", "INCR", "DECR", "INCRBY", "DECRBY", "INCRBYFLOAT",
    "APPEND", "SETBIT", "SETRANGE", "GETSET", "GETDEL", "EXPIRE",
    "PEXPIRE", "EXPIREAT", "PEXPIREAT", "PERSIST", "FLUSHDB",
    "FLUSHALL", "HSET", "HDEL", "HINCRBY", "HINCRBYFLOAT", "HSETNX",
    "HPEXPIREAT", "HPERSIST",
    "RPUSH", "LPUSH", "LPOP", "RPOP", "LSET", "LREM", "LTRIM",
    "LINSERT", "RENAME", "RENAMENX", "SADD", "SREM", "SPOP", "ZADD",
    "ZREM", "ZINCRBY", "ZPOPMIN", "ZREMRANGEBYRANK", "ZREMRANGEBYSCORE",
];
