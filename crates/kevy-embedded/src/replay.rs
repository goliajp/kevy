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
use kevy_store::Store;
use std::time::Duration;

/// Apply one command from an AOF frame to `store`. Mirrors the verbs
/// `Aof::rewrite_from` + the server's write-path AOF logging emit:
/// SET / HSET / RPUSH / SADD / ZADD / PEXPIRE — plus the
/// "append-side" verbs (DEL, FLUSHALL, EXPIRE, PERSIST, INCR family,
/// LPUSH, LPOP, RPOP, HDEL, SREM, ZREM, LSET, LREM, LTRIM, SPOP) that
/// the server's `KevyCommands::is_write` set logs as-is.
pub(crate) fn apply(store: &mut Store, args: &Argv) {
    let Some(name) = args.first() else { return };
    let verb = ascii_upper(name);
    match verb.as_slice() {
        b"SET" => apply_set(store, args),
        b"DEL" => {
            let keys: Vec<Vec<u8>> = args.iter().skip(1).map(|a| a.to_vec()).collect();
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
        b"PERSIST" => {
            if let Some(k) = args.get(1) {
                store.persist(k);
            }
        }
        b"FLUSHDB" | b"FLUSHALL" => store.flush(),
        b"HSET" => apply_hset(store, args),
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

fn apply_zadd(store: &mut Store, args: &Argv) {
    let Some(k) = args.get(1) else { return };
    let mut pairs: Vec<(f64, Vec<u8>)> = Vec::new();
    let mut i = 2;
    while i + 1 < args.len() {
        if let Some(score) = parse_f64(&args[i]) {
            pairs.push((score, args[i + 1].to_vec()));
        }
        i += 2;
    }
    if !pairs.is_empty() {
        let _ = store.zadd(k, &pairs);
    }
}

/// Common shape: `VERB key item1 item2 ...` → call `f(store, key, &items)`.
fn apply_pairs_strip<F>(store: &mut Store, args: &Argv, f: F)
where
    F: FnOnce(&mut Store, &[u8], &[Vec<u8>]),
{
    let Some(k) = args.get(1) else { return };
    let rest: Vec<Vec<u8>> = args.iter().skip(2).map(|a| a.to_vec()).collect();
    if !rest.is_empty() {
        f(store, k, &rest);
    }
}

fn ascii_upper(b: &[u8]) -> Vec<u8> {
    b.iter().map(|c| c.to_ascii_uppercase()).collect()
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
mod tests {
    use super::*;

    fn argv(parts: &[&[u8]]) -> Argv {
        Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
    }

    #[test]
    fn set_get_through_apply() {
        let mut s = Store::new();
        apply(&mut s, &argv(&[b"SET", b"k", b"v"]));
        assert_eq!(s.get(b"k").unwrap(), Some(&b"v"[..]));
    }

    #[test]
    fn all_basic_types_replay() {
        let mut s = Store::new();
        apply(&mut s, &argv(&[b"SET", b"str", b"hello"]));
        apply(&mut s, &argv(&[b"HSET", b"h", b"f1", b"v1", b"f2", b"v2"]));
        apply(&mut s, &argv(&[b"RPUSH", b"l", b"a", b"b", b"c"]));
        apply(&mut s, &argv(&[b"SADD", b"set", b"x", b"y"]));
        apply(&mut s, &argv(&[b"ZADD", b"z", b"1", b"a", b"2", b"b"]));
        apply(&mut s, &argv(&[b"PEXPIRE", b"str", b"60000"]));

        assert_eq!(s.dbsize(), 5);
        assert_eq!(s.type_of(b"str"), "string");
        assert_eq!(s.type_of(b"h"), "hash");
        assert_eq!(s.type_of(b"l"), "list");
        assert_eq!(s.type_of(b"set"), "set");
        assert_eq!(s.type_of(b"z"), "zset");
        assert!(s.pttl(b"str") > 50_000);
    }

    #[test]
    fn unknown_verb_is_silently_ignored() {
        let mut s = Store::new();
        apply(&mut s, &argv(&[b"FROBNICATE", b"x"]));
        assert_eq!(s.dbsize(), 0);
    }

    #[test]
    fn incrby_with_negative_replays() {
        let mut s = Store::new();
        apply(&mut s, &argv(&[b"INCRBY", b"n", b"5"]));
        apply(&mut s, &argv(&[b"INCRBY", b"n", b"3"]));
        apply(&mut s, &argv(&[b"DECRBY", b"n", b"4"]));
        assert_eq!(s.get(b"n").unwrap(), Some(&b"4"[..]));
    }
}
